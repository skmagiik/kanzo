//! Replies are short plain text to save agents' tokens.

use std::collections::HashSet;
use std::fmt::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::Result;
use axum::http::request::Parts;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Extensions, Implementation,
    ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::home::Home;
use crate::share::{self, Net};
use crate::store::{self, Account, Error, Kind, Project, Store, Task, TaskPatch, short_time};

/// An old board has thousands of done tasks.
const DONE_LISTED: usize = 20;

macro_rules! instructions {
    ($whose:literal, $project:literal) => {
        concat!(
            "The user's kanban board, on ",
            $whose,
            " machine, for all their projects. Tasks look like api#12. ",
            $project,
            " Keep it current unasked: doing when you start a task, comments on what you did, blocked when you \
             get stuck, review when you finish, url for a PR you opened. On review, comment how to resume this \
             session. Leave done and human tasks to the user. list_projects describes a project's own columns. \
             Link tasks as api#12, comments as api#12/c34 and files as [name](file:///path). If you were given a \
             Kanzo token, pass it as `token` on every call."
        )
    };
}

const INSTRUCTIONS: &str =
    instructions!("this", "Without `project`, Kanzo uses the project linked to this folder's GitHub repo.");
const HTTP_INSTRUCTIONS: &str =
    instructions!("their", "Over HTTP Kanzo can't see your folder, so pass `project` (see list_projects).");

const NO_PROJECTS: &str = "There are no projects yet. Create one with create_project.";

pub async fn serve(home: Home) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let repos = git_repos(&cwd);
    let images = home.images();
    let ctx = Ctx { home, cwd: Some(cwd), repos, images, host: Default::default() };
    let server = Kanzo { ctx: Arc::new(ctx), tool_router: Kanzo::tools() };
    // Our relay, not rmcp's stdio: it carries the session into a rebuilt or updated program.
    use rmcp::ServiceExt;
    let service = server.serve(crate::reload::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// `web::protect` checks Host and Origin, including when the board listens on the network,
/// so rmcp's localhost-only list is turned off.
pub fn http_service(home: Home, shutdown: CancellationToken) -> StreamableHttpService<Kanzo, LocalSessionManager> {
    let images = home.images();
    let ctx = Arc::new(Ctx { home, cwd: None, repos: Vec::new(), images, host: Default::default() });
    let config = StreamableHttpServerConfig::default().disable_allowed_hosts().with_cancellation_token(shutdown);
    // rmcp makes a server per session, and for some requests; the tool list is built once.
    let tool_router = Kanzo::tools();
    StreamableHttpService::new(
        move || Ok(Kanzo { ctx: ctx.clone(), tool_router: tool_router.clone() }),
        Default::default(),
        config,
    )
}

struct Ctx {
    home: Home,
    cwd: Option<PathBuf>,
    repos: Vec<String>,
    images: PathBuf,
    host: tokio::sync::Mutex<Option<share::Upstream>>,
}

impl Ctx {
    fn scope(&self, store: &Store, name: Option<&str>) -> store::Result<Option<Project>> {
        if name.is_some_and(|p| p.trim().eq_ignore_ascii_case("all")) {
            return Ok(None);
        }
        self.project(store, name).map(Some)
    }

    fn project(&self, store: &Store, name: Option<&str>) -> store::Result<Project> {
        if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
            return store.find_project(name);
        }
        for repo in &self.repos {
            if let Some(p) = store.project_by_repo(repo)? {
                return Ok(p);
            }
        }
        let known: Vec<String> = store.list_projects()?.into_iter().map(|p| p.slug).collect();
        let known = known.join(", ");
        let msg = match (self.cwd.as_ref().map(|c| c.display()), self.repos.first(), known.is_empty()) {
            (_, None, true) => NO_PROJECTS.to_string(),
            (None, ..) => {
                format!("Pass `project` (one of: {known}); over HTTP Kanzo cannot see which folder you work in.")
            }
            (Some(cwd), Some(repo), false) => format!(
                "No project is linked to {repo} (the git remote of {cwd}). Pass `project` (one of: {known}), \
                 link this repo to one with link_repo, or create a project for it with create_project and repo \"{repo}\"."
            ),
            (Some(cwd), Some(repo), true) => format!(
                "No project is linked to {repo} (the git remote of {cwd}), and there are no projects yet. \
                 Create one with create_project and repo \"{repo}\"."
            ),
            (Some(cwd), None, false) => {
                format!("Cannot tell the project: {cwd} has no git remote. Pass `project` (one of: {known}).")
            }
        };
        Err(Error::NotFound(msg))
    }
}

/// Without a token, Kanzo trusts the caller as it always has; a token's account may do only what its kind can.
fn caller(store: &Store, token: Option<&str>) -> store::Result<Option<Account>> {
    match token.map(str::trim).filter(|t| !t.is_empty()) {
        Some(token) => store.account_by_token(token).map(Some),
        None => Ok(None),
    }
}

fn kind(who: Option<&Account>) -> Option<Kind> {
    who.map(|a| a.kind)
}

fn refuse(who: Option<&Account>, what: &str) -> Error {
    let label = who.map_or("caller without a token", |a| a.kind.label());
    let article = if label.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    Error::Invalid(format!("{article} {label} can't {what}"))
}

/// Creating projects, linking repos and deleting tasks are the user's.
fn only_humans(who: Option<&Account>, what: &str) -> store::Result<()> {
    match kind(who) {
        Some(Kind::Orchestrator | Kind::Agent) => Err(refuse(who, what)),
        _ => Ok(()),
    }
}

/// An orchestrator made for one project works only in it.
fn own_project(store: &Store, who: Option<&Account>) -> store::Result<Option<Project>> {
    match who {
        Some(a) if a.kind == Kind::Orchestrator => a.project.as_deref().map(|p| store.find_project(p)).transpose(),
        _ => Ok(None),
    }
}

/// The project an orchestrator of one project names, which can only be its own.
fn within(own: Project, name: Option<&str>) -> store::Result<Project> {
    match name.map(str::trim).filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case("all")) {
        Some(n) if !n.eq_ignore_ascii_case(&own.slug) => {
            Err(Error::Invalid(format!("you are the orchestrator of {}, not of {n}", own.slug)))
        }
        _ => Ok(own),
    }
}

/// Agents see only their own tasks, orchestrators only their project's.
fn check_task(who: Option<&Account>, task: &Task) -> store::Result<()> {
    match who {
        Some(a) if a.kind == Kind::Agent && task.assignee.as_deref() != Some(a.name.as_str()) => {
            Err(Error::Invalid(format!("{} is not assigned to you", task.key())))
        }
        Some(a) if a.kind == Kind::Orchestrator && a.project.as_ref().is_some_and(|p| *p != task.project) => {
            Err(Error::Invalid(format!("{} is not in your project", task.key())))
        }
        Some(a) if a.kind != Kind::Human && task.human => {
            Err(Error::Invalid(format!("{} is for the user, not for agents", task.key())))
        }
        _ => Ok(()),
    }
}

fn find_task(store: &Store, who: Option<&Account>, key: &TaskKey) -> store::Result<Task> {
    let task = key.find(store)?;
    check_task(who, &task)?;
    Ok(task)
}

/// The project named, if any; "all" names none.
fn named_project(store: &Store, name: Option<&str>) -> store::Result<Option<Project>> {
    match name.map(str::trim).filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case("all")) {
        Some(n) => store.find_project(n).map(Some),
        None => Ok(None),
    }
}

fn reply(outcome: Result<String, String>) -> CallToolResult {
    match outcome {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(text) => CallToolResult::error(vec![ContentBlock::text(text)]),
    }
}

/// The client's name. Clients on protocol 2026-07-28 name themselves on every request; older
/// ones did once, in `initialize`.
fn client_name(context: &RequestContext<RoleServer>) -> Option<String> {
    let info = match context.meta.protocol_version() {
        Some(_) => context.meta.client_info(),
        None => context.peer.peer_info().map(|p| p.client_info.clone()),
    };
    info.map(|i| i.name).filter(|n| !n.is_empty())
}

fn git_repos(dir: &std::path::Path) -> Vec<String> {
    let Ok(text) = crate::home::output(Command::new("git").arg("-C").arg(dir).args(["remote", "-v"])) else {
        return Vec::new();
    };
    // `origin` first, so it wins when several remotes are linked to different projects.
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort_by_key(|l| !l.starts_with("origin\t"));
    let mut repos = Vec::new();
    for line in lines {
        if let Some(repo) = line.split_whitespace().nth(1).and_then(store::repo_from_url)
            && !repos.contains(&repo)
        {
            repos.push(repo);
        }
    }
    repos
}

#[derive(Clone)]
pub struct Kanzo {
    ctx: Arc<Ctx>,
    tool_router: ToolRouter<Self>,
}

/// Agents are told it is a string, which keeps the tool schemas short; a bare number is accepted too.
#[derive(Deserialize)]
#[serde(untagged)]
enum TaskKey {
    Number(i64),
    Text(String),
}

impl TaskKey {
    fn find(&self, store: &Store) -> store::Result<Task> {
        match self {
            TaskKey::Number(n) => store.get_task(*n),
            TaskKey::Text(s) => store.find_task(s),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct CreateProjectArgs {
    /// Short lowercase id, like api.
    slug: String,
    name: Option<String>,
    /// GitHub owner/repo.
    repo: Option<String>,
    /// Slug of a template project to copy columns and tasks from.
    template: Option<String>,
    token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ListTasksArgs {
    /// Slug, or "all".
    project: Option<String>,
    /// One column, or "all". Done is left out by default.
    status: Option<String>,
    /// Words, or tag: status: project: title: description: url: comment: id: comments: images: created: updated:
    /// is:human is:waiting has: no:, like tag:bug -status:done. Includes done.
    filter: Option<String>,
    token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct TaskArgs {
    /// Like api#12.
    #[schemars(with = "String")]
    id: TaskKey,
    token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct CreateTaskArgs {
    title: String,
    /// Markdown.
    description: Option<String>,
    /// Slug.
    project: Option<String>,
    /// Comma-separated.
    tags: Option<String>,
    /// Column; defaults to todo.
    status: Option<String>,
    /// For the user, not for agents.
    human: Option<bool>,
    /// Tasks it waits on, like api#12.
    waits_on: Option<String>,
    /// Like api#12: makes it a subtask of that task.
    parent: Option<String>,
    token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct UpdateTaskArgs {
    /// Like api#12.
    #[schemars(with = "String")]
    id: TaskKey,
    title: Option<String>,
    /// Markdown.
    description: Option<String>,
    /// Column, like review.
    status: Option<String>,
    /// Comma-separated; "" clears.
    tags: Option<String>,
    /// For the user, not for agents; true asks the user to review it.
    human: Option<bool>,
    /// A link; "" clears.
    url: Option<String>,
    /// Tasks it waits on, like api#12; "" clears.
    waits_on: Option<String>,
    /// An account's name, "AI Agents", or "" for nobody.
    assignee: Option<String>,
    /// Like api#12, or "" to make it a task of its own.
    parent: Option<String>,
    token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct AddCommentArgs {
    /// Like api#12.
    #[schemars(with = "String")]
    id: TaskKey,
    /// Markdown; may be "" with images.
    body: String,
    /// Images to attach, like screenshots: paths to PNG, JPEG, GIF or WebP files.
    images: Option<Vec<String>>,
    /// Defaults to your account, or your client's name.
    author: Option<String>,
    token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct LinkRepoArgs {
    /// Slug.
    project: String,
    /// GitHub owner/repo.
    repo: String,
    token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct NextTaskArgs {
    /// Slug, or "all".
    project: Option<String>,
    /// As in list_tasks, like tag:bug.
    filter: Option<String>,
    token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct AssignTaskArgs {
    /// Like api#12.
    #[schemars(with = "String")]
    id: TaskKey,
    /// Account name; by default a free task agent of the project, or a new one.
    agent: Option<String>,
    token: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RegisterArgs {
    /// human, orchestrator or agent.
    kind: Option<String>,
    /// Defaults to "<project> Orchestrator" or "<project> Agent <n>".
    name: Option<String>,
    /// Slug.
    project: Option<String>,
    /// Yours, if you have one.
    token: Option<String>,
}

impl Kanzo {
    /// Drops rmcp's `$schema` line and the `null` of optional fields: agents read this list in
    /// every session, so each byte counts.
    fn tools() -> ToolRouter<Self> {
        let mut router = Self::tool_router();
        for route in router.map.values_mut() {
            let mut schema = (*route.attr.input_schema).clone();
            schema.remove("$schema");
            // Optional fields are typed `["string", "null"]`; leaving them out of
            // `required` already says they're optional.
            if let Some(serde_json::Value::Object(props)) = schema.get_mut("properties") {
                for prop in props.values_mut() {
                    if let Some(kind) = prop.get_mut("type")
                        && let Some(first) = kind.as_array().and_then(|k| k.first()).cloned()
                    {
                        *kind = first;
                    }
                }
            }
            route.attr.input_schema = Arc::new(schema);
        }
        router
    }
}

#[tool_router]
impl Kanzo {
    async fn run(&self, f: impl FnOnce(&Ctx, &Store) -> store::Result<String> + Send + 'static) -> CallToolResult {
        self.run_for(&Extensions::default(), f).await
    }

    async fn run_for(
        &self,
        ext: &Extensions,
        f: impl FnOnce(&Ctx, &Store) -> store::Result<String> + Send + 'static,
    ) -> CallToolResult {
        let header = |name| {
            let value = ext.get::<Parts>()?.headers.get(name)?;
            Some(String::from_utf8_lossy(value.as_bytes()).into_owned())
        };
        let ctx = match header(share::FOLDER_HEADER) {
            Some(folder) => Arc::new(Ctx {
                home: self.ctx.home.clone(),
                cwd: Some(folder.into()),
                repos: header(share::REPOS_HEADER)
                    .unwrap_or_default()
                    .split(',')
                    .filter(|r| !r.is_empty())
                    .map(String::from)
                    .collect(),
                images: header(share::IMAGES_HEADER).map_or_else(|| self.ctx.images.clone(), PathBuf::from),
                host: Default::default(),
            }),
            None => self.ctx.clone(),
        };
        let outcome = tokio::task::spawn_blocking(move || {
            let store = ctx.home.store().map_err(|e| e.to_string())?;
            f(&ctx, &store).map_err(|e| e.to_string())
        })
        .await
        .unwrap_or_else(|e| Err(format!("internal error: {e}")));
        reply(outcome)
    }

    #[tool(description = "List projects with their repos and open task counts.")]
    async fn list_projects(&self, ext: Extensions) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run_for(&ext, |ctx, store| {
                let projects = store.list_projects()?;
                if projects.is_empty() {
                    return Ok(NO_PROJECTS.into());
                }
                let here = projects.iter().find(|p| p.repos.iter().any(|r| ctx.repos.contains(r)));
                let columns = store.custom_statuses(None)?;
                let mut out = String::new();
                for p in &projects {
                    out.push_str(&p.slug);
                    if p.name != p.slug {
                        let _ = write!(out, " \"{}\"", p.name);
                    }
                    if !p.repos.is_empty() {
                        let _ = write!(out, " ({})", p.repos.join(", "));
                    }
                    let _ = write!(out, ": {} open of {}", p.open, p.total);
                    if here.is_some_and(|h| h.id == p.id) {
                        out.push_str(" <- current repo");
                    }
                    if p.template {
                        out.push_str(" [template: agents don't take its tasks]");
                    }
                    out.push('\n');
                    for c in columns.iter().filter(|c| c.project == p.slug) {
                        let _ = writeln!(out, "  column {}: {}", c.name, c.description);
                    }
                }
                out.pop();
                Ok(out)
            })
            .await)
    }

    #[tool(description = "Create a project, optionally from a template project.")]
    async fn create_project(
        &self,
        Parameters(args): Parameters<CreateProjectArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |_, store| {
                only_humans(caller(store, args.token.as_deref())?.as_ref(), "create projects")?;
                let name = args.name.as_deref().unwrap_or("");
                let p = match args.template.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                    Some(template) => store.create_project_from(&args.slug, name, args.repo.as_deref(), template)?,
                    None => store.create_project(&args.slug, name, args.repo.as_deref())?,
                };
                let repo = p.repos.first().map(|r| format!(", linked to {r}")).unwrap_or_default();
                let tasks =
                    if p.total > 0 { format!(" with {} {}", p.total, store::tasks(p.total)) } else { String::new() };
                Ok(format!("Created project {} ({}){repo}{tasks}.", p.slug, p.name))
            })
            .await)
    }

    #[tool(description = "Link another GitHub repo to a project.")]
    async fn link_repo(&self, Parameters(args): Parameters<LinkRepoArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |_, store| {
                only_humans(caller(store, args.token.as_deref())?.as_ref(), "link repos")?;
                let p = store.link_repo(&args.project, &args.repo)?;
                Ok(format!("Project {} is linked to {}.", p.slug, p.repos.join(", ")))
            })
            .await)
    }

    #[tool(description = "List tasks in board order; a task agent sees only its own.")]
    async fn list_tasks(
        &self,
        Parameters(args): Parameters<ListTasksArgs>,
        ext: Extensions,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run_for(&ext, move |ctx, store| {
                let who = caller(store, args.token.as_deref())?;
                let who = who.as_ref();
                let (project, assignee) = match (kind(who), own_project(store, who)?) {
                    (Some(Kind::Agent), _) => (named_project(store, args.project.as_deref())?, who.map(|a| a.id)),
                    (_, Some(own)) => (Some(within(own, args.project.as_deref())?), None),
                    _ => (ctx.scope(store, args.project.as_deref())?, None),
                };
                let status = args.status.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
                let only = match status.as_deref() {
                    Some("all") | None => None,
                    Some(s) => {
                        store.check_status(project.as_ref().map(|p| p.id), s)?;
                        Some(s)
                    }
                };
                let filter = args.filter.filter(|f| !f.trim().is_empty());
                let keep = match (&filter, &status) {
                    (Some(_), _) => None,
                    (None, None) => Some(0),
                    (None, Some(_)) => Some(DONE_LISTED),
                };
                let (mut tasks, hidden) =
                    store.list_tasks_keeping_done(project.as_ref().map(|p| p.id), only, assignee, keep)?;
                if let Some(filter) = &filter {
                    let found: HashSet<i64> = store.search_for_agent(filter)?.into_iter().map(|t| t.id).collect();
                    tasks.retain(|t| found.contains(&t.id));
                }
                let done = store::DONE;
                let properties = store.hidden_properties()?;
                let mut out = String::new();
                let mut people = 0;
                for t in &tasks {
                    if t.human {
                        people += 1;
                    } else {
                        task_line(&mut out, t, &properties);
                        out.push('\n');
                    }
                }
                if out.is_empty() {
                    let scope = project.as_ref().map_or("any project", |p| p.slug.as_str());
                    let yours = if assignee.is_some() { " assigned to you" } else { "" };
                    let _ = writeln!(out, "No tasks{yours} in {scope}.");
                }
                if people > 0 && shown(&properties, "human") {
                    let _ = writeln!(
                        out,
                        "({people} human {} not shown: for the user, not for agents)",
                        store::tasks(people)
                    );
                }
                if hidden > 0 && status.is_none() {
                    let _ = writeln!(out, "({hidden} {done} not shown; pass status \"{done}\" or \"all\")");
                } else if hidden > 0 {
                    let _ = writeln!(out, "({hidden} older {done} not shown)");
                }
                out.pop();
                Ok(out)
            })
            .await)
    }

    #[tool(description = "Show a task with its comments and image paths.")]
    async fn get_task(
        &self,
        Parameters(args): Parameters<TaskArgs>,
        ext: Extensions,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run_for(&ext, move |ctx, store| {
                let who = caller(store, args.token.as_deref())?;
                let task = find_task(store, who.as_ref(), &args.id)?;
                task_detail(ctx, store, &task)
            })
            .await)
    }

    #[tool(description = "Add a task, to todo unless you pass status.")]
    async fn create_task(
        &self,
        Parameters(args): Parameters<CreateTaskArgs>,
        ext: Extensions,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run_for(&ext, move |ctx, store| {
                let CreateTaskArgs { title, description, project, tags, status, human, waits_on, parent, token } = args;
                let who = caller(store, token.as_deref())?;
                let who = who.as_ref();
                let status = status.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
                match kind(who) {
                    Some(Kind::Agent) => return Err(refuse(who, "create tasks")),
                    Some(Kind::Orchestrator) if status.as_deref() == Some(store::DONE) => {
                        return Err(refuse(who, "create done tasks"));
                    }
                    _ => {}
                }
                let project = match own_project(store, who)? {
                    Some(own) => within(own, project.as_deref())?,
                    None => ctx.project(store, project.as_deref())?,
                };
                let parent = match parent.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
                    Some(key) => Some(find_task(store, who, &TaskKey::Text(key.into()))?.id),
                    None => None,
                };
                let patch = TaskPatch {
                    description,
                    status,
                    tags,
                    human,
                    waits_on,
                    parent: parent.map(Some),
                    ..Default::default()
                };
                let t = store.create_task_in(project.id, &title, patch)?;
                Ok(format!("Created {} in {}: {}{}", t.key(), t.status, t.title, waits_note(store, &t)?))
            })
            .await)
    }

    #[tool(description = "Change the fields you pass. A task agent can set status (doing, blocked, review or a \
                          project's own column) and hand its task back: assignee \"\" or a person, or human true.")]
    async fn update_task(
        &self,
        Parameters(args): Parameters<UpdateTaskArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = client_name(&context);
        Ok(self
            .run(move |_, store| {
                let who = caller(store, args.token.as_deref())?;
                let who = who.as_ref();
                store.acting_as(&actor(who, None, client));
                let before = find_task(store, who, &args.id)?;
                let status = args.status.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
                let assignee = match args.assignee.as_deref().map(str::trim) {
                    None => None,
                    Some("") => Some((store::Assignee::Nobody, None)),
                    Some(n) if n.eq_ignore_ascii_case(store::AI_AGENTS) => Some((store::Assignee::Agents, None)),
                    Some(n) => {
                        let a = store.find_account(n)?;
                        Some((store::Assignee::Account(a.id), Some(a)))
                    }
                };
                let to = assignee.as_ref().map(|(_, a)| a.as_ref());
                let parent = match args.parent.as_deref().map(str::trim) {
                    None => None,
                    Some("") => Some(None),
                    Some(key) => Some(Some(find_task(store, who, &TaskKey::Text(key.into()))?.id)),
                };
                match kind(who) {
                    Some(Kind::Agent) => {
                        if args.title.is_some() || args.description.is_some() || args.tags.is_some() || parent.is_some()
                        {
                            return Err(refuse(who, "edit tasks; comment instead"));
                        }
                        if args.human == Some(false) {
                            return Err(refuse(who, "take a task back from the user"));
                        }
                        // It can only hand its task back: to nobody, or to a person.
                        if let Some(to) = to
                            && !matches!(assignee, Some((store::Assignee::Nobody, _)))
                            && to.is_none_or(|a| a.kind != Kind::Human)
                        {
                            return Err(refuse(who, "assign tasks; hand yours back with assignee \"\" or a person"));
                        }
                        if let Some(s) = status.as_deref() {
                            let project = store.find_project(&before.project)?;
                            let mut allowed =
                                vec![store::DOING.to_string(), store::BLOCKED.into(), store::REVIEW.into()];
                            allowed.extend(store.custom_statuses(Some(project.id))?.into_iter().map(|c| c.name));
                            if !allowed.iter().any(|a| a == s) {
                                return Err(refuse(who, &format!("move tasks there; use {}", allowed.join(", "))));
                            }
                        }
                    }
                    Some(Kind::Orchestrator) => {
                        if status.as_deref() == Some(store::DONE) {
                            return Err(refuse(who, "mark tasks done; pass human true to ask the user to review it"));
                        }
                        if args.human == Some(false) {
                            return Err(refuse(who, "take a task back from the user"));
                        }
                        if let Some(Some(a)) = to {
                            if a.kind == Kind::Orchestrator {
                                return Err(Error::Invalid(format!("{} is an orchestrator, not a task agent", a.name)));
                            }
                            if let Some(p) = a.project.as_ref().filter(|p| **p != before.project) {
                                return Err(Error::Invalid(format!("{} works on {p}, not {}", a.name, before.project)));
                            }
                        }
                    }
                    _ => {}
                }
                let after = store.update_task(
                    before.id,
                    TaskPatch {
                        title: args.title,
                        description: args.description,
                        status,
                        tags: args.tags,
                        project_id: None,
                        human: args.human,
                        url: args.url,
                        waits_on: args.waits_on,
                        assignee: assignee.map(|(a, _)| a),
                        parent,
                    },
                )?;
                let mut moved = if before.status != after.status {
                    format!(", {} -> {}", before.status, after.status)
                } else {
                    String::new()
                };
                if after.human && !before.human {
                    moved.push_str(", now for the user");
                } else if before.human && !after.human {
                    moved.push_str(", now for agents");
                }
                if after.assignee != before.assignee {
                    match &after.assignee {
                        Some(a) => moved.push_str(&format!(", now assigned to {a}")),
                        None => moved.push_str(", now unassigned"),
                    }
                }
                Ok(format!("Updated {}{moved}: {}{}", after.key(), after.title, waits_note(store, &after)?))
            })
            .await)
    }

    #[tool(description = "Comment on a task, like to report progress. Attach screenshots with images.")]
    async fn add_comment(
        &self,
        Parameters(args): Parameters<AddCommentArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = client_name(&context);
        Ok(self
            .run(move |ctx, store| {
                let who = caller(store, args.token.as_deref())?;
                let task = find_task(store, who.as_ref(), &args.id)?;
                let author = actor(who.as_ref(), args.author, client);
                let files = save_images(ctx, &args.images.unwrap_or_default())?;
                store.add_comment(task.id, &author, &args.body, &files)?;
                let images = match files.len() {
                    0 => String::new(),
                    1 => " with an image".into(),
                    n => format!(" with {n} images"),
                };
                Ok(format!("Comment added to {}{images} as {author}.", task.key()))
            })
            .await)
    }

    #[tool(description = "Take the top todo task, move it to doing and show it. A task agent gets its own; an \
                          orchestrator hands the top task queued for AI Agents to a task agent.")]
    async fn next_task(
        &self,
        Parameters(args): Parameters<NextTaskArgs>,
        ext: Extensions,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = client_name(&context);
        Ok(self
            .run_for(&ext, move |ctx, store| {
                let who = caller(store, args.token.as_deref())?;
                let who = who.as_ref();
                store.acting_as(&actor(who, None, client));
                let (project, assignee) = match kind(who) {
                    Some(Kind::Agent) => (named_project(store, args.project.as_deref())?, who.map(|a| a.id)),
                    Some(Kind::Orchestrator) => {
                        let project = match own_project(store, who)? {
                            Some(own) => within(own, args.project.as_deref())?,
                            None => ctx.project(store, args.project.as_deref())?,
                        };
                        let Some(task) = store.queued_for_agents(project.id)? else {
                            return Ok(format!(
                                "Nothing in todo for {} is queued for {}.",
                                project.slug,
                                store::AI_AGENTS
                            ));
                        };
                        let (agent, created) = store.assign_agent(task.id)?;
                        let task = store.get_task(task.id)?;
                        return Ok(format!(
                            "{}\nStart it with token {} and task {}.\n\n{}",
                            assigned(&task, &agent, created),
                            agent.token,
                            task.key(),
                            task_detail(ctx, store, &task)?
                        ));
                    }
                    _ => (ctx.scope(store, args.project.as_deref())?, None),
                };
                let id = project.as_ref().map(|p| p.id);
                let filter = args.filter.as_deref().unwrap_or("");
                match store.next_task_matching(id, assignee, filter)? {
                    Some(task) => Ok(format!("Moved to {}:\n{}", task.status, task_detail(ctx, store, &task)?)),
                    None => {
                        let waiting = store.waiting_in_todo(id, assignee, filter)?;
                        let scope = project.map_or("any project".into(), |p| p.slug);
                        let yours = if assignee.is_some() { " assigned to you" } else { "" };
                        Ok(match waiting {
                            0 => format!("Nothing in todo{yours} for {scope}."),
                            1 => format!("Nothing ready in todo{yours} for {scope}: 1 task waits on others."),
                            n => format!("Nothing ready in todo{yours} for {scope}: {n} tasks wait on others."),
                        })
                    }
                }
            })
            .await)
    }

    #[tool(description = "Delete a task with its comments and images. Can't be undone.")]
    async fn delete_task(&self, Parameters(args): Parameters<TaskArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(move |ctx, store| {
                only_humans(caller(store, args.token.as_deref())?.as_ref(), "delete tasks")?;
                let task = args.id.find(store)?;
                let properties = store.hidden_properties()?;
                ctx.home.deleting_images(store, store::ImagesOf::Task(task.id), || store.delete_task(task.id))?;
                let count = if shown(&properties, "comments") { task.comments } else { 0 };
                let comments = match count {
                    0 => String::new(),
                    1 => " and its comment".into(),
                    n => format!(" and its {n} comments"),
                };
                Ok(format!("Deleted {}{comments}: {}", task.key(), task.title))
            })
            .await)
    }

    #[tool(description = "Assign a task to a task agent, by default a free or new one of its project. Replies with \
                          the agent's token.")]
    async fn assign_task(
        &self,
        Parameters(args): Parameters<AssignTaskArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let client = client_name(&context);
        Ok(self
            .run(move |_, store| {
                let who = caller(store, args.token.as_deref())?;
                let who = who.as_ref();
                store.acting_as(&actor(who, None, client));
                if kind(who) == Some(Kind::Agent) {
                    return Err(refuse(who, "assign tasks"));
                }
                let task = find_task(store, who, &args.id)?;
                if task.status == store::DONE {
                    return Err(Error::Invalid(format!("{} is done", task.key())));
                }
                if kind(who) == Some(Kind::Orchestrator)
                    && let Some(name) = task.assignee.as_deref().filter(|a| *a != store::AI_AGENTS)
                    && store.find_account(name)?.kind == Kind::Human
                {
                    return Err(Error::Invalid(format!("{} is assigned to {name}, a person", task.key())));
                }
                let (agent, created) = match args.agent.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                    Some(name) => {
                        let agent = store.find_account(name)?;
                        if kind(who) == Some(Kind::Orchestrator) {
                            if agent.kind != Kind::Agent {
                                return Err(Error::Invalid(format!("{} is not a task agent", agent.name)));
                            }
                            if let Some(p) = agent.project.as_ref().filter(|p| **p != task.project) {
                                return Err(Error::Invalid(format!(
                                    "{} works on {p}, not {}",
                                    agent.name, task.project
                                )));
                            }
                        }
                        let patch =
                            TaskPatch { assignee: Some(store::Assignee::Account(agent.id)), ..Default::default() };
                        store.update_task(task.id, patch)?;
                        (agent, false)
                    }
                    None => store.assign_agent(task.id)?,
                };
                Ok(format!(
                    "{}, token {}\nStart the agent with its token and the task id.",
                    assigned(&task, &agent, created),
                    agent.token
                ))
            })
            .await)
    }

    #[tool(description = "Make an account and its token. Without a token, only an orchestrator; an orchestrator \
                          makes task agents; only humans make humans.")]
    async fn register(
        &self,
        Parameters(args): Parameters<RegisterArgs>,
        ext: Extensions,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run_for(&ext, move |ctx, store| {
                let who = caller(store, args.token.as_deref())?;
                let who = who.as_ref();
                let new = match args.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
                    Some(k) => Kind::parse(k)?,
                    None if kind(who) == Some(Kind::Orchestrator) => Kind::Agent,
                    None => Kind::Orchestrator,
                };
                let allowed = match kind(who) {
                    None => new == Kind::Orchestrator,
                    Some(Kind::Human) => true,
                    Some(Kind::Orchestrator) => new == Kind::Agent,
                    Some(Kind::Agent) => false,
                };
                if !allowed {
                    return Err(refuse(who, &format!("register a {}", new.label())));
                }
                let project = match (new, own_project(store, who)?) {
                    (Kind::Human, _) => None,
                    (_, Some(own)) => Some(within(own, args.project.as_deref())?),
                    _ if args.project.is_some() => named_project(store, args.project.as_deref())?,
                    _ => ctx.project(store, None).ok(),
                };
                let name = args.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
                let account = match (name, new, &project) {
                    (Some(name), ..) => store.create_account(name, new, project.as_ref().map(|p| p.id))?,
                    (None, Kind::Agent, Some(p)) => store.create_agent(p)?,
                    (None, Kind::Orchestrator, Some(p)) => {
                        store.create_account(&format!("{} Orchestrator", p.name), new, Some(p.id))?
                    }
                    _ => return Err(Error::Invalid("pass a name, or a project to name the account after".into())),
                };
                let scope = account.project.as_deref().map(|p| format!(" for {p}")).unwrap_or_default();
                Ok(format!(
                    "Registered {} ({}{scope}), token {}\nPass it as `token` on every Kanzo call.",
                    account.name,
                    account.kind.label(),
                    account.token
                ))
            })
            .await)
    }
}

// Serve the router built once in `tools()`; by default the macro builds a new,
// untrimmed one for every request.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Kanzo {
    /// Checked on every call, so agents already running switch boards when this computer joins or leaves.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // Over HTTP, the board itself sends calls on when it joined another.
        if let Some(cwd) = &self.ctx.cwd
            && let Net::Joined(host) = Net::load(&self.ctx.home)
        {
            let mut upstream = self.ctx.host.lock().await;
            if upstream.as_ref().is_none_or(|u| u.host() != host) {
                *upstream = Some(share::Upstream::new(host, cwd, &self.ctx.repos, &self.ctx.images));
            }
            let client = share::handshake(context.peer.peer_info().as_deref(), &context.meta);
            let upstream = upstream.as_mut().expect("set above");
            let mut args = request.arguments;
            // The shared board can't read this computer's files: images go up first, by name.
            if request.name == "add_comment"
                && let Some(Value::Array(images)) = args.as_mut().and_then(|a| a.get_mut("images"))
            {
                for entry in images.iter_mut() {
                    let Some(path) = entry.as_str() else { continue };
                    let path = resolve(Some(cwd), path);
                    let saved = match read_image(&path) {
                        Ok((bytes, ext)) => upstream.upload_image(bytes, ext).await,
                        Err(e) => Err(e.to_string()),
                    };
                    match saved {
                        Ok(file) => *entry = Value::String(file),
                        Err(e) => return Ok(reply(Err(e)).into()),
                    }
                }
            }
            let outcome = upstream.call(client, &request.name, args).await;
            return Ok(reply(outcome).into());
        }
        self.tool_router.call(ToolCallContext::new(self, request, context)).await
    }

    fn get_info(&self) -> ServerConfig {
        // A rebuilt Kanzo may bring other tools; see `reload`.
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_tool_list_changed().build())
            .with_server_info(Implementation::new("kanzo", env!("CARGO_PKG_VERSION")))
            .with_instructions(if self.ctx.cwd.is_some() { INSTRUCTIONS } else { HTTP_INSTRUCTIONS })
    }
}

fn shown(hidden: &[String], property: &str) -> bool {
    !hidden.iter().any(|p| p == property)
}

/// A path an agent passed, from the folder it works in.
fn resolve(cwd: Option<&PathBuf>, path: &str) -> PathBuf {
    let path = PathBuf::from(path.trim());
    match cwd {
        Some(cwd) if path.is_relative() => cwd.join(path),
        _ => path,
    }
}

fn read_image(path: &std::path::Path) -> store::Result<(Vec<u8>, &'static str)> {
    let shown = path.display();
    let size = std::fs::metadata(path).map_err(|e| Error::Invalid(format!("cannot read {shown}: {e}")))?.len();
    if size > store::IMAGE_LIMIT as u64 {
        return Err(Error::Invalid(format!("{shown} is larger than {} MB", store::IMAGE_LIMIT >> 20)));
    }
    let bytes = std::fs::read(path).map_err(|e| Error::Invalid(format!("cannot read {shown}: {e}")))?;
    match store::image_type(&bytes) {
        Some(ext) => Ok((bytes, ext)),
        None => Err(Error::Invalid(format!("{shown} isn't a PNG, JPEG, GIF or WebP image"))),
    }
}

/// Saves the images attached to a comment and returns their file names. An entry is a path, or the
/// name of an image the board already has, as a joined computer sends it.
fn save_images(ctx: &Ctx, entries: &[String]) -> store::Result<Vec<String>> {
    let dir = ctx.home.images();
    entries
        .iter()
        .map(|entry| {
            let entry = entry.trim();
            if store::valid_image_file(entry) && dir.join(entry).is_file() {
                return Ok(entry.to_string());
            }
            let (bytes, ext) = read_image(&resolve(ctx.cwd.as_ref(), entry))?;
            ctx.home.save_image(&bytes, ext).map_err(|e| Error::Invalid(format!("cannot save {entry}: {e}")))
        })
        .collect()
}

/// Who comments and moves are by: an account is always itself; without one, the name the
/// agent gave, then its client's.
fn actor(who: Option<&Account>, named: Option<String>, client: Option<String>) -> String {
    match who {
        Some(a) => a.name.clone(),
        None => named.filter(|a| !a.trim().is_empty()).or(client).unwrap_or_else(|| "agent".into()),
    }
}

fn assigned(task: &Task, agent: &Account, created: bool) -> String {
    let new = if created { "new task agent " } else { "" };
    format!("Assigned {} ({}) to {new}{}", task.key(), task.status, agent.name)
}

fn task_line(out: &mut String, t: &Task, hidden: &[String]) {
    let _ = write!(out, "{} [{}", t.key(), t.status);
    if let Some(a) = &t.assignee {
        let _ = write!(out, " @{a}");
    }
    let _ = write!(out, "] {}", t.title);
    if let Some(p) = &t.parent {
        let _ = write!(out, " (sub of {p})");
    }
    if shown(hidden, "tags") {
        write_tags(out, t);
    }
    if shown(hidden, "waiting") {
        write_waits(out, t);
    }
    match if shown(hidden, "comments") { t.comments } else { 0 } {
        0 => {}
        1 => out.push_str(" (1 comment)"),
        n => {
            let _ = write!(out, " ({n} comments)");
        }
    }
}

fn write_tags(out: &mut String, t: &Task) {
    if !t.tags.is_empty() {
        let _ = write!(out, " {{{}}}", t.tags.join(", "));
    }
}

fn write_waits(out: &mut String, t: &Task) {
    if !t.waiting_on.is_empty() {
        let ids: Vec<String> = t.waiting_on.iter().map(|id| format!("#{id}")).collect();
        let _ = write!(out, " (waits on {})", ids.join(", "));
    }
}

/// Tells an agent that moved or changed a task that it's still waiting; moving one anyway is allowed.
fn waits_note(store: &Store, t: &Task) -> store::Result<String> {
    let mut note = String::new();
    if !t.waiting_on.is_empty() && shown(&store.hidden_properties()?, "waiting") {
        write_waits(&mut note, t);
    }
    Ok(note)
}

fn task_detail(ctx: &Ctx, store: &Store, t: &Task) -> store::Result<String> {
    let hidden = store.hidden_properties()?;
    let comments = if shown(&hidden, "comments") { store.list_comments(t.id)? } else { Vec::new() };
    let images = if shown(&hidden, "images") { store.list_images(t.id)? } else { Vec::new() };
    let dir = &ctx.images;
    let paths = |comment: Option<i64>| -> Vec<String> {
        images.iter().filter(|i| i.comment_id == comment).map(|i| dir.join(&i.file).display().to_string()).collect()
    };
    let (created, updated) = (short_time(&t.created_at), short_time(&t.updated_at));
    let mut out = format!("{} {}", t.key(), t.title);
    if shown(&hidden, "tags") {
        write_tags(&mut out, t);
    }
    let _ = write!(out, "\nstatus: {}", t.status);
    if shown(&hidden, "waiting") {
        write_waits(&mut out, t);
    }
    if let Some(a) = &t.assignee {
        let _ = write!(out, ", assigned to {a}");
    }
    if let Some(p) = &t.parent {
        let _ = write!(out, ", subtask of {p}");
    }
    if t.human && shown(&hidden, "human") {
        out.push_str(", human: for the user, not for agents");
    }
    if shown(&hidden, "created") {
        let _ = write!(out, ", created {created}");
    }
    if updated != created && shown(&hidden, "updated") {
        let _ = write!(out, ", updated {updated}");
    }
    if !t.url.is_empty() && shown(&hidden, "url") {
        let _ = write!(out, "\nurl: {}", t.url);
    }
    out.push('\n');
    if !t.description.is_empty() && shown(&hidden, "description") {
        let _ = writeln!(out, "\n{}", t.description);
    }
    let own = paths(None);
    if !own.is_empty() {
        let _ = writeln!(out, "\nImages: {}", own.join(", "));
    }
    let subtasks = store.subtasks(t.id)?;
    if !subtasks.is_empty() {
        let done = subtasks.iter().filter(|s| s.status == store::DONE).count();
        let _ = writeln!(out, "\nSubtasks ({done}/{} done):", subtasks.len());
        for mut s in subtasks {
            // Listed under its parent, it needn't name it again.
            s.parent = None;
            out.push_str("- ");
            task_line(&mut out, &s, &hidden);
            out.push('\n');
        }
    }
    if !comments.is_empty() {
        let _ = writeln!(out, "\nComments ({}):", t.comments);
        for c in &comments {
            if c.is_note() {
                let _ = writeln!(out, "- {} by {}, {}", c.body, c.author, short_time(&c.created_at));
                continue;
            }
            let _ = write!(out, "- c{} {}, {}: {}", c.id, c.author, short_time(&c.created_at), c.body);
            let attached = paths(Some(c.id));
            if !attached.is_empty() {
                let sep = if c.body.is_empty() { "" } else { " " };
                let _ = write!(out, "{sep}[images: {}]", attached.join(", "));
            }
            out.push('\n');
        }
    }
    out.truncate(out.trim_end().len());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tokio::io::{
        AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
    };

    use rmcp::ServiceExt;

    use super::*;

    const STATELESS: &str = "2026-07-28";

    /// The client side of a `kanzo mcp` on stdio.
    struct Agent {
        lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
        write: WriteHalf<DuplexStream>,
    }

    impl Agent {
        fn start(home: Home, folder: &std::path::Path) -> Self {
            let images = home.images();
            let ctx = Ctx { home, cwd: Some(folder.into()), repos: Vec::new(), images, host: Default::default() };
            let server = Kanzo { ctx: Arc::new(ctx), tool_router: Kanzo::tools() };
            let (ours, theirs) = tokio::io::duplex(1 << 16);
            tokio::spawn(async move { server.serve(tokio::io::split(theirs)).await.unwrap().waiting().await });
            let (read, write) = tokio::io::split(ours);
            Self { lines: BufReader::new(read).lines(), write }
        }

        /// The reply to `message`, or `None` for a notification.
        async fn send(&mut self, message: Value) -> Option<Value> {
            self.write.write_all(format!("{message}\n").as_bytes()).await.unwrap();
            let id = message.get("id")?;
            loop {
                let reply: Value = serde_json::from_str(&self.lines.next_line().await.unwrap().unwrap()).unwrap();
                if &reply["id"] == id {
                    return Some(reply);
                }
            }
        }
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    fn board() -> (tempfile::TempDir, Home, Task) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path().to_path_buf()).unwrap();
        let store = home.store().unwrap();
        let project = store.create_project("api", "", None).unwrap();
        let task = store.create_task(project.id, "Ship it", "").unwrap();
        (dir, home, task)
    }

    /// Serves `home`'s MCP over HTTP on a free port, as a shared board does.
    async fn shared(home: Home) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new().nest_service("/mcp", http_service(home, CancellationToken::new()));
        tokio::spawn(async move { axum::serve(listener, app).await });
        port
    }

    fn authors(home: &Home, task: &Task) -> Vec<String> {
        home.store().unwrap().list_comments(task.id).unwrap().into_iter().map(|c| c.author).collect()
    }

    fn comment(task: &Task, meta: Value) -> Value {
        let args = json!({ "id": task.key(), "body": "done" });
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "add_comment", "arguments": args, "_meta": meta } })
    }

    fn stateless(client: Option<&str>) -> Value {
        let mut meta = json!({
            "io.modelcontextprotocol/protocolVersion": STATELESS,
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        if let Some(name) = client {
            meta["io.modelcontextprotocol/clientInfo"] = json!({ "name": name, "version": "1" });
        }
        meta
    }

    #[test]
    fn agents_see_what_a_task_waits_on_and_why_nothing_is_ready() {
        block_on(async {
            let (dir, home, blocker) = board();
            let mut agent = Agent::start(home.clone(), dir.path());
            let mut call = async |name: &str, args: Value| -> String {
                let params = json!({ "name": name, "arguments": args, "_meta": stateless(None) });
                let message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params });
                let reply = agent.send(message).await.unwrap();
                reply["result"]["content"][0]["text"].as_str().unwrap().to_string()
            };
            let waiting =
                call("create_task", json!({ "title": "Then this", "project": "api", "waits_on": blocker.key() })).await;
            assert_eq!(waiting, "Created api#2 in todo: Then this (waits on #1)");
            assert!(call("list_tasks", json!({ "project": "api" })).await.contains("Then this (waits on #1)"));
            assert!(call("get_task", json!({ "id": "api#2" })).await.contains("status: todo (waits on #1)"));

            call("update_task", json!({ "id": "api#1", "status": "doing" })).await;
            let next = call("next_task", json!({ "project": "api" })).await;
            assert_eq!(next, "Nothing ready in todo for api: 1 task waits on others.");
            let moved = call("update_task", json!({ "id": "api#2", "status": "doing" })).await;
            assert_eq!(moved, "Updated api#2, todo -> doing: Then this (waits on #1)");
            let next = call("next_task", json!({ "project": "api" })).await;
            assert_eq!(next, "Nothing in todo for api.");

            home.store().unwrap().set_property("waiting", false).unwrap();
            assert!(!call("get_task", json!({ "id": "api#2" })).await.contains("waits on"));
        });
    }

    #[test]
    fn a_joined_board_takes_calls_from_a_client_that_never_initialized() {
        block_on(async {
            let (_host_dir, host, task) = board();
            let port = shared(host.clone()).await;
            let guest_dir = tempfile::tempdir().unwrap();
            std::fs::write(guest_dir.path().join(share::FILE), format!("join 127.0.0.1:{port}\n")).unwrap();
            let mut agent = Agent::start(Home::open(guest_dir.path().to_path_buf()).unwrap(), guest_dir.path());

            let reply = agent.send(comment(&task, stateless(Some("claude-code")))).await.unwrap();
            assert_eq!(reply["result"]["isError"], false, "{reply}");
            assert_eq!(authors(&host, &task), ["claude-code"]);
        });
    }

    #[test]
    fn comments_are_signed_with_the_name_a_client_initialized_with() {
        block_on(async {
            let (dir, home, task) = board();
            let mut agent = Agent::start(home.clone(), dir.path());
            let client = json!({ "name": "codex-mcp-client", "version": "1" });
            let params = json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client });
            agent.send(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": params })).await;
            agent.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;

            let meta = json!({ "io.modelcontextprotocol/clientInfo": { "name": "from-meta", "version": "1" } });
            agent.send(comment(&task, meta)).await;
            assert_eq!(authors(&home, &task), ["codex-mcp-client"]);
        });
    }

    #[test]
    fn a_comment_over_http_from_a_client_that_gives_no_name_is_signed_agent() {
        block_on(async {
            let (_dir, home, task) = board();
            let port = shared(home.clone()).await;
            let body = comment(&task, stateless(None)).to_string();
            let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let head = format!(
                "POST /mcp HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
                 Accept: application/json, text/event-stream\r\nMCP-Protocol-Version: {STATELESS}\r\n\
                 Mcp-Method: tools/call\r\nMcp-Name: add_comment\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            conn.write_all(format!("{head}{body}").as_bytes()).await.unwrap();
            let mut reply = String::new();
            conn.read_to_string(&mut reply).await.unwrap();

            assert!(reply.contains("Comment added"), "{reply}");
            assert_eq!(authors(&home, &task), ["agent"]);
        });
    }
}
