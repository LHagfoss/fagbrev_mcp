use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use chromiumoxide::{
    Browser,
    browser::BrowserConfig,
    cdp::browser_protocol::{
        browser::{SetDownloadBehaviorBehavior, SetDownloadBehaviorParams},
        input::InsertTextParams,
    },
};
use directories::ProjectDirs;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::time::{sleep, timeout};

use crate::attachments::{MAX_ATTACHMENT_BYTES, finalize_download, validate_output_path};

use crate::data::{
    AttachmentDownloadResult, AttachmentList, AttachmentMetadata, AttachmentSource, CompetencyGoal,
    CompetencyGoalDetails, CompetencyGoalReference, DashboardOverview, DeleteDocumentationResult,
    Delmal, DocumentationPage, DocumentationRecord, DocumentationStatus, DocumentationTarget,
    DocumentationTargetSummary, FeedbackRecord, HalfYearTask, LinkedDocumentationSummary,
    RequestApprovalResult,
};

const FAGBREV_URL: &str = "https://fagbrev.io/l";
const PLAN_URL: &str = "https://fagbrev.io/l/laereplanmal";
const DOCUMENTATION_URL: &str = "https://fagbrev.io/l/dokumentasjon";
const LOGIN_WAIT: Duration = Duration::from_secs(10 * 60);
const SHARED_BROWSER_IDLE: Duration = Duration::from_secs(10 * 60);
const SHARED_PAGE_WAIT: Duration = Duration::from_secs(5 * 60);
const DOCUMENTATION_PAGE_SIZES: [u32; 6] = [6, 12, 18, 24, 100, 200];
const HALF_YEAR_TASK_TITLES: [&str; 6] = [
    "Etikk, lovverk og yrkesutøvelse",
    "Kodeferdigheter og metode",
    "Sikkerhet og personvern",
    "Infrastruktur og arkitektur",
    "Design, interaksjon og brukerdialog",
    "Minifagprøve",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PageSnapshot {
    title: String,
    url: String,
    text: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DocumentationRowSnapshot {
    id: Option<String>,
    url: Option<String>,
    title: Option<String>,
    updated_at: Option<String>,
    status_icon: Option<String>,
    status_color: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct DocumentationListSnapshot {
    page_size: Option<u32>,
    total: Option<u32>,
    rows: Vec<DocumentationRowSnapshot>,
}

#[derive(Debug, Clone, Deserialize)]
struct DocumentationTableReadiness {
    has_table: bool,
    linked_rows: u32,
    body_text: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DocumentationDetailSnapshot {
    title: Option<String>,
    status_label: Option<String>,
    updated_at: Option<String>,
    content_text: Option<String>,
    content_html: Option<String>,
    attachments: Vec<AttachmentMetadata>,
    target_text: Option<String>,
    feedback: Vec<FeedbackSnapshot>,
}

#[derive(Debug, Clone, Deserialize)]
struct FeedbackSnapshot {
    status_label: Option<String>,
    status_icon: Option<String>,
    status_color: Option<String>,
    author: Option<String>,
    timestamp: Option<String>,
    text: String,
    #[serde(default)]
    attachments: Vec<AttachmentMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
struct HalfYearTaskSnapshot {
    ordinal: u8,
    title: String,
    task_text: Option<String>,
    status_label: Option<String>,
    status_icon: Option<String>,
    status_color: Option<String>,
    documentation_count: Option<u32>,
    linked_documentation: Vec<LinkedDocumentationSummary>,
}

#[derive(Debug, Clone, Deserialize)]
struct UiMutationResult {
    ok: bool,
    #[serde(default)]
    missing: Vec<String>,
    #[serde(default)]
    message: Option<String>,
}

fn app_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("io", "fagbrev", "fagbrev-mcp")
        .context("could not determine an OS application-data directory")
}

fn profile_dir() -> Result<PathBuf> {
    Ok(app_dirs()?.data_dir().join("browser-profile"))
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn browser_config(headful: bool) -> Result<BrowserConfig> {
    let profile = profile_dir()?;
    std::fs::create_dir_all(&profile)
        .with_context(|| format!("could not create browser profile at {}", profile.display()))?;

    let mut builder = BrowserConfig::builder()
        .user_data_dir(profile)
        .port(0)
        .window_size(1280, 900)
        .args([
            "--disable-background-timer-throttling",
            "--disable-backgrounding-occluded-windows",
            "--disable-renderer-backgrounding",
        ]);

    if let Ok(executable) = env::var("FAGBREV_BROWSER_EXECUTABLE") {
        builder = builder.chrome_executable(executable);
    }

    if headful {
        builder = builder.with_head();
    }

    builder
        .build()
        .map_err(|error| anyhow::anyhow!("could not build Chromium configuration: {error}"))
}

async fn launch(headful: bool) -> Result<(Browser, chromiumoxide::Handler)> {
    if let Ok(cdp_url) = env::var("FAGBREV_CDP_URL") {
        if cdp_url.trim().is_empty() {
            bail!("FAGBREV_CDP_URL is set but empty");
        }
        tracing::info!(%cdp_url, "attaching to an existing browser over CDP");
        return Browser::connect(cdp_url).await.map_err(Into::into);
    }

    Browser::launch(browser_config(headful)?)
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "could not launch Chromium: {error}. Install Chrome/Chromium or set FAGBREV_CDP_URL to an existing browser's WebSocket URL"
            )
        })
}

async fn page_snapshot(page: &chromiumoxide::Page) -> Result<PageSnapshot> {
    let title = page.get_title().await?.unwrap_or_default();
    let url = page.url().await?.unwrap_or_default();
    let text: String = page
        .evaluate("() => document.body ? document.body.innerText : ''")
        .await?
        .into_value()?;

    Ok(PageSnapshot { title, url, text })
}

async fn wait_for_text(page: &chromiumoxide::Page, needle: &str) -> Result<PageSnapshot> {
    let mut snapshot = page_snapshot(page).await?;
    for _ in 0..20 {
        if snapshot.text.contains(needle) {
            return Ok(snapshot);
        }
        sleep(Duration::from_millis(500)).await;
        snapshot = page_snapshot(page).await?;
    }
    Ok(snapshot)
}

fn is_authenticated(snapshot: &PageSnapshot) -> bool {
    let text = snapshot.text.to_lowercase();
    let url = snapshot.url.to_lowercase();
    (url.contains("fagbrev.io/l") || url.contains("fagbrev.io/"))
        && (text.contains("velkommen") || text.contains("opplæringsplan"))
        && !text.contains("telefon eller e-post")
}

fn number_after(text: &str, marker: &str) -> Option<u32> {
    let words = text.split_whitespace().collect::<Vec<_>>();
    let index = words
        .iter()
        .position(|word| word.trim_matches(|c: char| !c.is_alphabetic()) == marker)?;
    words
        .get(index + 1)?
        .trim_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()
}

fn number_before_phrase(text: &str, phrase: &str) -> Option<u32> {
    let words = text.split_whitespace().collect::<Vec<_>>();
    let phrase_words = phrase.split_whitespace().collect::<Vec<_>>();
    let start = words
        .windows(phrase_words.len())
        .position(|window| window == phrase_words)?;
    words
        .get(start.checked_sub(1)?)?
        .trim_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()
}

fn dashboard_overview_from_snapshot(snapshot: PageSnapshot) -> DashboardOverview {
    DashboardOverview {
        authenticated: is_authenticated(&snapshot),
        page_title: snapshot.title,
        url: snapshot.url,
        progress_percent: number_after(&snapshot.text, "fullført")
            .and_then(|value| u8::try_from(value).ok()),
        documentations_written: number_after(&snapshot.text, "skrevet"),
        assessment_conversations: number_after(&snapshot.text, "hatt"),
        documentations_needing_correction: number_before_phrase(
            &snapshot.text,
            "Dokumentasjoner som må rettes",
        ),
        documentations_to_review: number_before_phrase(
            &snapshot.text,
            "Dokumentasjoner til vurdering",
        ),
        approved_documentations: number_before_phrase(&snapshot.text, "Godkjente dokumentasjoner"),
    }
}

fn documentation_status_from_ui(
    label: Option<&str>,
    icon: Option<&str>,
    color: Option<&str>,
) -> DocumentationStatus {
    let label = label.unwrap_or_default().trim().to_lowercase();
    let icon = icon.unwrap_or_default().trim().to_lowercase();
    let color = color.unwrap_or_default().trim().to_lowercase();

    if label == "sendt" {
        return DocumentationStatus::Submitted;
    }
    if label == "godkjent" || icon == "check-circle" || color == "bg-positive" {
        return DocumentationStatus::Approved;
    }
    if label == "må rettes"
        || label == "trenger endring"
        || (icon == "edit-2" && color == "bg-negative")
    {
        return DocumentationStatus::NeedsCorrection;
    }
    if label == "til vurdering" || icon == "clock" {
        return DocumentationStatus::InReview;
    }
    if label == "kladd" || (icon == "edit" && color == "bg-dark") {
        return DocumentationStatus::Draft;
    }
    if label == "avvist" {
        return DocumentationStatus::Rejected;
    }

    DocumentationStatus::Unknown
}

fn parse_feedback(document_id: &str, entries: Vec<FeedbackSnapshot>) -> Vec<FeedbackRecord> {
    entries
        .into_iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let text = entry.text.trim().to_string();
            (!text.is_empty()).then_some(FeedbackRecord {
                ordinal: index as u32 + 1,
                document_id: document_id.to_string(),
                status: Some(documentation_status_from_ui(
                    entry.status_label.as_deref(),
                    entry.status_icon.as_deref(),
                    entry.status_color.as_deref(),
                ))
                .filter(|status| *status != DocumentationStatus::Unknown),
                author: entry
                    .author
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                timestamp: entry
                    .timestamp
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                text,
                attachments: entry.attachments,
            })
        })
        .collect()
}

fn parse_half_year_tasks(entries: Vec<HalfYearTaskSnapshot>) -> Vec<HalfYearTask> {
    entries
        .into_iter()
        .map(|entry| {
            let status = documentation_status_from_ui(
                entry.status_label.as_deref(),
                entry.status_icon.as_deref(),
                entry.status_color.as_deref(),
            );
            HalfYearTask {
                ordinal: entry.ordinal,
                title: entry.title,
                task_text: entry
                    .task_text
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                status: (status != DocumentationStatus::Unknown).then_some(status),
                documentation_count: entry.documentation_count,
                linked_documentation: entry.linked_documentation,
            }
        })
        .collect()
}

fn parse_documentation_rows(rows: Vec<DocumentationRowSnapshot>) -> Vec<DocumentationRecord> {
    rows.into_iter()
        .map(|row| DocumentationRecord {
            id: row.id,
            url: row.url,
            title: row.title,
            updated_at: row.updated_at,
            status: documentation_status_from_ui(
                None,
                row.status_icon.as_deref(),
                row.status_color.as_deref(),
            ),
            ..DocumentationRecord::default()
        })
        .collect()
}

fn parse_target_summary(text: Option<&str>) -> Option<DocumentationTargetSummary> {
    let text = text?.trim();
    if text.is_empty() {
        return None;
    }

    let mut summary = DocumentationTargetSummary {
        summary_text: Some(text.to_string()),
        ..DocumentationTargetSummary::default()
    };
    let mut current_goal = None;
    let mut saw_target_data = false;

    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if line.eq_ignore_ascii_case("LÆREPLAN") {
            continue;
        }
        if summary.learning_plan.is_none() && line.starts_with("Læreplan") {
            summary.learning_plan = Some(line.to_string());
            continue;
        }
        if line.contains("Kompetansemål") {
            summary.category = Some(line.to_string());
            continue;
        }
        if line.contains("mål og") && line.contains("delmål") {
            continue;
        }
        if line.eq_ignore_ascii_case("Ingen kompetansemål tilknyttet") {
            continue;
        }
        if let Some((number, title)) = numbered_heading(line)
            && (1..=21).contains(&number)
        {
            summary.competency_goals.push(CompetencyGoalReference {
                number,
                title: Some(title.to_string()),
                id: None,
            });
            current_goal = Some(number);
            saw_target_data = true;
            continue;
        }
        if line.ends_with("dok.") || line.starts_with("Halvårsoppgaver") {
            continue;
        }
        if let Some(goal_number) = current_goal {
            summary.delmal.push(Delmal {
                goal_number,
                number: None,
                title: line.to_string(),
                id: None,
                status: None,
                status_count: None,
            });
            saw_target_data = true;
        }
    }

    (saw_target_data || summary.learning_plan.is_some() || summary.category.is_some())
        .then_some(summary)
}

fn documentation_record_from_detail(
    id: String,
    url: String,
    snapshot: DocumentationDetailSnapshot,
) -> DocumentationRecord {
    DocumentationRecord {
        id: Some(id.clone()),
        url: Some(url),
        title: snapshot.title,
        content: snapshot
            .content_text
            .filter(|value| !value.trim().is_empty()),
        content_html: snapshot
            .content_html
            .filter(|value| !value.trim().is_empty()),
        // Detail-page icons can belong to feedback or other unrelated controls.
        // Only an explicit status label is safe evidence for the document itself.
        status: documentation_status_from_ui(snapshot.status_label.as_deref(), None, None),
        updated_at: snapshot.updated_at,
        attachments: snapshot.attachments,
        target_summary: parse_target_summary(snapshot.target_text.as_deref()),
        feedback: parse_feedback(&id, snapshot.feedback),
        ..DocumentationRecord::default()
    }
}

/// The browser behind one tool call. One-shot CLI commands own their browser;
/// the MCP servers share one headless browser and only close their own page.
enum BrowserHandle {
    Owned(Box<Browser>),
    Shared {
        browser: Arc<Browser>,
        page: chromiumoxide::Page,
        _turn: tokio::sync::MutexGuard<'static, ()>,
    },
}

impl std::ops::Deref for BrowserHandle {
    type Target = Browser;

    fn deref(&self) -> &Browser {
        match self {
            Self::Owned(browser) => browser,
            Self::Shared { browser, .. } => browser,
        }
    }
}

type Session = (
    BrowserHandle,
    Option<tokio::task::JoinHandle<()>>,
    chromiumoxide::Page,
    bool,
);

struct SharedBrowser {
    browser: Arc<Browser>,
    handler_task: tokio::task::JoinHandle<()>,
    last_used: Instant,
}

static SHARED_BROWSER_ENABLED: AtomicBool = AtomicBool::new(false);
static SHARED_BROWSER: tokio::sync::Mutex<Option<SharedBrowser>> =
    tokio::sync::Mutex::const_new(None);
/// One shared-browser page at a time: Chrome throttles background tabs so
/// hard that parallel tool calls took minutes instead of seconds.
static SHARED_PAGE_TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
// Serialize complete write workflows, including their separate read-back calls.
// This cannot prevent concurrent changes made outside this server process.
static DOCUMENTATION_WRITE_TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Reuse one headless browser for every tool call in this process.
///
/// Launching Chrome per call dominated tool latency, and parallel calls each
/// tried to lock the same profile. The browser is closed after
/// `SHARED_BROWSER_IDLE` without calls and relaunched on demand.
pub fn enable_shared_browser() {
    if SHARED_BROWSER_ENABLED.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async {
        loop {
            sleep(Duration::from_secs(60)).await;
            let mut guard = SHARED_BROWSER.lock().await;
            let idle = guard.as_ref().is_some_and(|shared| {
                shared.last_used.elapsed() >= SHARED_BROWSER_IDLE
                    && Arc::strong_count(&shared.browser) == 1
            });
            if idle && let Some(shared) = guard.take() {
                close_shared_browser(shared).await;
            }
        }
    });
}

pub async fn shutdown_shared_browser() {
    if let Some(shared) = SHARED_BROWSER.lock().await.take() {
        close_shared_browser(shared).await;
    }
}

async fn close_shared_browser(shared: SharedBrowser) {
    if let Ok(mut browser) = Arc::try_unwrap(shared.browser) {
        let _ = browser.close().await;
        let _ = browser.wait().await;
    }
    shared.handler_task.abort();
}

fn spawn_handler(mut handler: chromiumoxide::Handler) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if let Err(error) = event {
                tracing::debug!(%error, "browser handler stopped");
                break;
            }
        }
    })
}

async fn shared_browser() -> Result<Arc<Browser>> {
    let mut guard = SHARED_BROWSER.lock().await;
    if let Some(shared) = guard.as_mut()
        && !shared.handler_task.is_finished()
        && shared.browser.version().await.is_ok()
    {
        shared.last_used = Instant::now();
        return Ok(shared.browser.clone());
    }
    if let Some(stale) = guard.take() {
        close_shared_browser(stale).await;
    }
    let (browser, handler) = launch(false).await?;
    let browser = Arc::new(browser);
    *guard = Some(SharedBrowser {
        browser: browser.clone(),
        handler_task: spawn_handler(handler),
        last_used: Instant::now(),
    });
    Ok(browser)
}

async fn open_dashboard(headful: bool) -> Result<Session> {
    open_session(headful, FAGBREV_URL).await
}

/// Open a page directly at `url`. With an attached CDP browser the current
/// page is reused and navigated instead.
async fn open_page(url: &str) -> Result<Session> {
    let session = open_session(false, url).await?;
    if env::var_os("FAGBREV_CDP_URL").is_some() {
        session.2.goto(url).await?;
    }
    Ok(session)
}

async fn open_session(headful: bool, url: &str) -> Result<Session> {
    let owns_browser = env::var_os("FAGBREV_CDP_URL").is_none();
    if owns_browser && !headful && SHARED_BROWSER_ENABLED.load(Ordering::SeqCst) {
        let turn = timeout(SHARED_PAGE_WAIT, SHARED_PAGE_TURN.lock())
            .await
            .context("timed out waiting for another Fagbrev browser call to finish")?;
        let browser = shared_browser().await?;
        // An interrupted call may have released the turn without closing its
        // page. No other shared call can use these tabs while we hold the turn.
        for stale_page in browser.pages().await? {
            stale_page
                .close()
                .await
                .context("could not close a stale Fagbrev browser page")?;
        }
        let page = browser.new_page(url).await?;
        return Ok((
            BrowserHandle::Shared {
                browser,
                page: page.clone(),
                _turn: turn,
            },
            None,
            page,
            false,
        ));
    }

    let (mut browser, handler) = launch(headful).await?;
    let handler_task = Some(spawn_handler(handler));
    let page = if owns_browser {
        browser.new_page(url).await?
    } else {
        browser.fetch_targets().await?;
        sleep(Duration::from_millis(250)).await;
        let mut pages = browser.pages().await?;
        if let Some(page) = pages.pop() {
            page
        } else {
            bail!("connected browser has no open pages")
        }
    };
    Ok((
        BrowserHandle::Owned(Box::new(browser)),
        handler_task,
        page,
        owns_browser,
    ))
}

async fn plan_page(headful: bool) -> Result<Session> {
    let session = open_session(headful, PLAN_URL).await?;
    if env::var_os("FAGBREV_CDP_URL").is_none() {
        let snapshot = wait_for_text(&session.2, "KOMPETANSEMÅL").await?;
        if !snapshot.text.contains("KOMPETANSEMÅL") {
            finish(session.0, session.1, session.3).await?;
            bail!("the competency-goal tab did not finish loading");
        }
    } else if !session
        .2
        .url()
        .await?
        .unwrap_or_default()
        .contains("/l/laereplanmal")
    {
        bail!("attached browser is not on the læreplan page; navigate there first");
    }
    Ok(session)
}

async fn half_year_page(headful: bool) -> Result<Session> {
    let session = plan_page(headful).await?;
    wait_for_text(&session.2, "HALVÅRSOPPGAVER").await?;
    let selected = session
        .2
        .evaluate(
            "() => { const tab = Array.from(document.querySelectorAll('[role=\"tab\"], button')).find(element => (element.textContent || '').replace(/\\s+/g, ' ').toLowerCase().includes('halvårsoppgaver')); if (!tab) return false; if (tab.getAttribute('aria-selected') !== 'true') tab.click(); return true; }",
        )
        .await?
        .into_value::<bool>()?;
    if !selected {
        let current = page_snapshot(&session.2).await?;
        finish(session.0, session.1, session.3).await?;
        bail!(
            "could not find the half-year assignments tab on {} ({})",
            current.url,
            current.title
        );
    }
    let snapshot = wait_for_text(&session.2, "Minifagprøve").await?;
    if !snapshot.text.contains("Minifagprøve") {
        finish(session.0, session.1, session.3).await?;
        bail!("the half-year assignments tab did not finish loading");
    }
    Ok(session)
}

async fn half_year_tasks_snapshot(page: &chromiumoxide::Page) -> Result<Vec<HalfYearTaskSnapshot>> {
    let titles = serde_json::to_string(&HALF_YEAR_TASK_TITLES)?;
    page.evaluate(format!(
        r#"() => {{
            const titles = {titles};
            const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
            const buttons = Array.from(document.querySelectorAll('main button.FExpantion-trigger'));
            titles.forEach(expected => {{
                const button = buttons.find(candidate => {{
                    const heading = candidate.querySelector('.FExpantion-header');
                    const title = normalize((heading?.innerText || candidate.innerText).replace(/^\s*\d+\s*/, ''));
                    return title === expected;
                }});
                if (button && !button.parentElement?.querySelector('.FExpantion-arrow-open')) button.click();
            }});
            return true;
        }}"#
    ))
    .await?
    .into_value::<bool>()?;
    sleep(Duration::from_millis(400)).await;
    page.evaluate(format!(
        r#"() => {{
            const titles = {titles};
            const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
            const buttons = Array.from(document.querySelectorAll('main button.FExpantion-trigger'));
            return titles.map((expected, index) => {{
                const button = buttons.find(candidate => {{
                    const heading = candidate.querySelector('.FExpantion-header');
                    const title = normalize((heading?.innerText || candidate.innerText).replace(/^\s*\d+\s*/, ''));
                    return title === expected;
                }});
                if (!button) return null;
                const root = button.parentElement;
                const heading = button.querySelector('.FExpantion-header');
                const title = normalize((heading?.innerText || button.innerText).replace(/^\s*\d+\s*/, ''));
                const statusIcon = root?.querySelector('.FExpantion-header i[data-name], .FExpantion-header i[data-type]');
                const documentationButton = Array.from(root?.querySelectorAll('button') || [])
                    .find(candidate => normalize(candidate.innerText).startsWith('Se tilknyttede dokumentasjoner'));
                const documentationMatch = documentationButton?.innerText.match(/\((\d+)\)/);
                const editor = root?.querySelector('.FExpantion-content .ql-editor');
                const linked = Array.from(root?.querySelectorAll('a[href^="/l/dokumentasjon/"]') || [])
                    .map(anchor => {{
                        const href = anchor.getAttribute('href');
                        const match = href?.match(/^\/l\/dokumentasjon\/([^/?#]+)$/);
                        return {{ id: match ? match[1] : null, title: normalize(anchor.innerText) || null, status: null }};
                    }});
                return {{
                    ordinal: index + 1,
                    title,
                    task_text: editor?.innerText?.trim() || null,
                    status_label: null,
                    status_icon: statusIcon?.getAttribute('data-name') || statusIcon?.getAttribute('data-type') || null,
                    status_color: null,
                    documentation_count: documentationMatch ? Number(documentationMatch[1]) : null,
                    linked_documentation: linked
                }};
            }}).filter(Boolean);
        }}"#
    ))
    .await?
    .into_value()
    .map_err(Into::into)
}

fn parse_goals_from_page_text(text: &str) -> Vec<CompetencyGoal> {
    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let headings = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let (number, title) = numbered_heading(line)?;
            (1..=21).contains(&number).then_some((index, number, title))
        })
        .collect::<Vec<_>>();
    let count_before = headings
        .iter()
        .filter(|(index, _, _)| {
            index
                .checked_sub(1)
                .and_then(|previous| lines[previous].parse::<u32>().ok())
                .is_some()
        })
        .count();
    let count_after = headings
        .iter()
        .filter(|(index, _, _)| {
            lines
                .get(index + 1)
                .and_then(|line| line.parse::<u32>().ok())
                .is_some()
        })
        .count();
    let count_after_heading = count_after > count_before;
    let mut goals = Vec::new();

    for (index, number, title) in headings {
        // The plan has rendered the count both before and after the heading
        // in different UI states. Looking only for a preceding count made
        // the authenticated list silently become [].
        let status_count = if count_after_heading {
            lines
                .get(index + 1)
                .and_then(|line| line.parse::<u32>().ok())
        } else {
            index
                .checked_sub(1)
                .and_then(|previous| lines[previous].parse::<u32>().ok())
        };
        let Some(status_count) = status_count else {
            continue;
        };

        if goals
            .iter()
            .any(|goal: &CompetencyGoal| goal.number == number)
        {
            continue;
        }
        goals.push(CompetencyGoal {
            number,
            title: title.to_string(),
            status_count,
            id: None,
            status: None,
        });
    }

    goals
}

fn goal_list_is_complete(goals: &[CompetencyGoal]) -> bool {
    goals.len() == 21 && (1..=21).all(|number| goals.iter().any(|goal| goal.number == number))
}

fn goal_list_error(goals: &[CompetencyGoal], snapshot: &PageSnapshot) -> anyhow::Error {
    anyhow::anyhow!(
        "could not parse the 21 competency goals from the authenticated læreplan page: found {} numbered goals (page title: {:?}, url: {:?}); the Fagbrev UI may still be loading or its goal layout may have changed",
        goals.len(),
        snapshot.title,
        snapshot.url
    )
}

/// The smallest page-size option exposed by the documentation table that can
/// satisfy a bounded read. The caller still trims the returned rows to its
/// requested limit.
pub(crate) fn documentation_page_size_for_limit(limit: u32) -> u32 {
    DOCUMENTATION_PAGE_SIZES
        .into_iter()
        .find(|page_size| *page_size >= limit)
        .unwrap_or(200)
}

fn numbered_heading(line: &str) -> Option<(u8, &str)> {
    let (number, title) = line.split_once(". ")?;
    let number = number.parse::<u8>().ok()?;
    let title = title.trim();
    (!title.is_empty()).then_some((number, title))
}

fn push_expanded_delmal(goal_number: u8, delmal: &mut Vec<Delmal>, current: &mut Option<String>) {
    if let Some(title) = current.take()
        && !title.ends_with(':')
    {
        delmal.push(Delmal {
            goal_number,
            number: Some(delmal.len() as u16 + 1),
            title,
            id: None,
            status: None,
            status_count: None,
        });
    }
}

fn parse_expanded_goal_from_page_text(
    text: &str,
    goal_number: u8,
) -> (Option<String>, Option<u32>, Vec<Delmal>) {
    let mut title = None;
    let mut status_count = None;
    let mut in_goal = false;
    let mut delmal = Vec::new();
    let mut current_title: Option<String> = None;
    let mut pending_status_count = None;
    let mut in_delmal_list = false;
    let mut pending_delmal_count = None;

    for raw_line in text.lines().filter(|line| !line.trim().is_empty()) {
        let line = raw_line.trim();
        if let Some((number, heading)) = numbered_heading(line) {
            if number == goal_number {
                in_goal = true;
                title = Some(heading.to_string());
                status_count = pending_status_count.take();
                current_title = None;
                continue;
            }
            if in_goal && (1..=21).contains(&number) {
                break;
            }
        }

        if !in_goal {
            if let Ok(value) = line.parse::<u32>() {
                pending_status_count = Some(value);
            }
            continue;
        }

        if line.starts_with("Se tilknyttede dokumentasjoner")
            || line == "Ny dokumentasjon"
            || line.starts_with("Godkjent ")
            || line.starts_with("Til vurdering")
            || line.starts_with("Trenger endring")
            || line.starts_with("Last ned ")
        {
            break;
        }

        // The rendered "Delmål ( N )" list is authoritative. Bullets above it
        // belong to the goal description ("Forslag til oppgaver"), so drop them.
        if is_delmal_list_heading(line) {
            in_delmal_list = true;
            delmal.clear();
            current_title = None;
            continue;
        }
        if in_delmal_list {
            if let Ok(count) = line.parse::<u32>() {
                pending_delmal_count = Some(count);
            } else {
                delmal.push(Delmal {
                    goal_number,
                    number: Some(delmal.len() as u16 + 1),
                    title: line.to_string(),
                    id: None,
                    status: None,
                    status_count: pending_delmal_count.take(),
                });
            }
            continue;
        }

        if current_title.is_none() && line.parse::<u32>().is_ok() {
            status_count = line.parse::<u32>().ok();
            continue;
        }

        if let Some(activity) = line.strip_prefix("* ") {
            push_expanded_delmal(goal_number, &mut delmal, &mut current_title);
            let activity = activity.trim();
            if !activity.ends_with(':') {
                current_title = Some(activity.to_string());
            }
            continue;
        }

        if raw_line.chars().next().is_some_and(char::is_whitespace) {
            if let Some(current) = current_title.as_mut() {
                current.push(' ');
                current.push_str(line);
            }
        } else {
            // Unbulleted, non-indented text is a section heading or prose,
            // not another part of the preceding work activity.
            push_expanded_delmal(goal_number, &mut delmal, &mut current_title);
        }
    }

    push_expanded_delmal(goal_number, &mut delmal, &mut current_title);

    (title, status_count, delmal)
}

fn is_delmal_list_heading(line: &str) -> bool {
    line.strip_prefix("Delmål")
        .map(str::trim)
        .and_then(|rest| rest.strip_prefix('('))
        .and_then(|rest| rest.strip_suffix(')'))
        .is_some_and(|count| count.trim().parse::<u32>().is_ok())
}

async fn competency_goal_attachment_snapshot(
    page: &chromiumoxide::Page,
    goal_number: u8,
    delmal_title: Option<&str>,
) -> Result<Vec<AttachmentMetadata>> {
    let delmal_title = serde_json::to_string(&delmal_title)?;
    page.evaluate(format!(
        r#"() => {{
            const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
            const button = Array.from(document.querySelectorAll('main button')).find(element => new RegExp('(?:^|\\s){}\\.\\s').test(normalize(element.innerText)));
            const root = button?.parentElement;
            const target = {delmal_title};
            const all = Array.from(root?.querySelectorAll('.FFileViewerBtn') || []);
            const selected = target ? all.filter(element => {{
                let node = element.parentElement;
                while (node && node !== root) {{
                    if (normalize(node.innerText).includes(target)) return true;
                    node = node.parentElement;
                }}
                return false;
            }}) : all;
            return selected.map((element, index) => {{
                const clickable = element.matches('a,button') ? element : element.querySelector('a,button') || element.closest('a,button');
                const rawUrl = clickable?.getAttribute('href') || clickable?.getAttribute('data-url') || element.getAttribute('data-url');
                let url = null;
                try {{ const parsed = rawUrl ? new URL(rawUrl, window.location.href) : null; url = parsed && (parsed.protocol === 'http:' || parsed.protocol === 'https:') ? parsed.href : null; }} catch (_) {{}}
                const name = (element.querySelector('.FFileViewerBtn-content span')?.innerText || clickable?.getAttribute('aria-label') || element.getAttribute('title') || element.innerText || '').trim() || null;
                const mime = clickable?.getAttribute('data-mime-type') || clickable?.getAttribute('data-mime') || element.getAttribute('data-mime-type') || null;
                const sizeValue = clickable?.getAttribute('data-size-bytes') || element.getAttribute('data-size-bytes');
                const parsedSize = sizeValue && /^\d+$/.test(sizeValue) ? Number(sizeValue) : null;
                return {{ordinal: index + 1, name, url, mime_type: mime, size_bytes: parsedSize}};
            }});
        }}"#,
        goal_number
    ))
    .await?
    .into_value()
    .map_err(Into::into)
}

/// Open the new-documentation form and wait until its target picker is usable.
///
/// Clicking "Skriv ny dokumentasjon" creates a blank draft and routes to its
/// editor. The returned draft ID lets callers report that draft instead of
/// silently leaving an untitled entry behind when a later step fails.
async fn documentation_picker_page() -> Result<(Session, Option<String>)> {
    let session = open_page(DOCUMENTATION_URL).await?;
    let page = &session.2;
    let opened = async {
        wait_for_text(page, "Skriv ny dokumentasjon").await?;
        page.evaluate(
            "() => { const button = Array.from(document.querySelectorAll('button')).find(button => (button.textContent || '').includes('Skriv ny dokumentasjon')); if (!button) return false; button.click(); return true; }",
        )
        .await?
        .into_value::<bool>()
        .map_err(anyhow::Error::from)
    }
    .await;
    match opened {
        Ok(true) => {}
        Ok(false) => {
            finish(session.0, session.1, session.3).await?;
            bail!("could not open the new documentation form");
        }
        Err(error) => {
            let _ = finish(session.0, session.1, session.3).await;
            return Err(with_blank_draft_hint(error, None));
        }
    }

    let mut draft_id = None;
    for _ in 0..40 {
        let url = match page.url().await {
            Ok(url) => url.unwrap_or_default(),
            Err(error) => {
                let _ = finish(session.0, session.1, session.3).await;
                return Err(with_blank_draft_hint(error.into(), None));
            }
        };
        if url.trim_end_matches('/').ends_with("/rediger") {
            draft_id = documentation_id_from_url(&url);
            break;
        }
        sleep(Duration::from_millis(250)).await;
    }

    let ready = async {
        wait_for_edit_form(page).await?;
        wait_for_target_picker(page).await
    }
    .await;
    if let Err(error) = ready {
        finish(session.0, session.1, session.3).await?;
        return Err(with_blank_draft_hint(error, draft_id.as_deref()));
    }
    Ok((session, draft_id))
}

fn with_blank_draft_hint(error: anyhow::Error, draft_id: Option<&str>) -> anyhow::Error {
    match draft_id {
        Some(id) => error.context(format!(
            "document {id} ({DOCUMENTATION_URL}/{id}) was created or may have been saved, but the operation is unconfirmed; inspect this ID and use update_documentation if needed, rather than creating another document"
        )),
        None => error.context(
            "opening the form may have created a blank \"Ingen tittel\" draft; check get_documentation before submitting again",
        ),
    }
}

/// Wait until every numbered competency goal is rendered as a picker label,
/// then reveal the delmål hidden behind "Vis N til" buttons.
async fn wait_for_target_picker(page: &chromiumoxide::Page) -> Result<()> {
    let mut goal_labels = 0;
    for _ in 0..40 {
        goal_labels = page
            .evaluate(
                "() => Array.from(document.querySelectorAll('main label')).filter(label => label.querySelector('.FCheckbox') && /^\\d+\\.\\s/.test((label.innerText || '').trim())).length",
            )
            .await?
            .into_value::<u32>()?;
        if goal_labels >= 21 {
            break;
        }
        sleep(Duration::from_millis(250)).await;
    }
    if goal_labels < 21 {
        bail!("the documentation target picker showed {goal_labels} of 21 competency goals");
    }

    for _ in 0..12 {
        let expanded: bool = page
            .evaluate(
                "() => { const buttons = Array.from(document.querySelectorAll('main button')).filter(button => /^Vis \\d+ til$/.test((button.innerText || '').replace(/\\s+/g, ' ').trim())); buttons.forEach(button => button.click()); return buttons.length > 0; }",
            )
            .await?
            .into_value()?;
        if !expanded {
            return Ok(());
        }
        sleep(Duration::from_millis(150)).await;
    }
    bail!("the documentation target picker kept showing \"Vis N til\" buttons")
}

async fn click_documentation_expansion(page: &chromiumoxide::Page, prefix: &str) -> Result<bool> {
    let prefix = serde_json::to_string(prefix)?;
    page.evaluate(format!(
        "() => {{ const button = Array.from(document.querySelectorAll('button')).find(button => (button.textContent || '').trim().startsWith({prefix})); if (!button) return false; button.click(); return true; }}"
    ))
    .await?
    .into_value()
    .map_err(Into::into)
}

async fn documentation_list_snapshot(
    page: &chromiumoxide::Page,
) -> Result<DocumentationListSnapshot> {
    page.evaluate(
        r#"() => {
            const rows = Array.from(document.querySelectorAll('table tr'))
                .filter(row => row.querySelectorAll('td').length >= 4)
                .map(row => {
                    const cells = row.querySelectorAll('td');
                    const href = row.querySelector('a[href^="/l/dokumentasjon/"]')?.getAttribute('href') || null;
                    const match = href && href.match(/^\/l\/dokumentasjon\/([^/?#]+)$/);
                    const icon = cells[0]?.querySelector('i[data-name], i[data-type]');
                    const statusContainer = cells[0]?.querySelector('[class*="bg-"]');
                    return {
                        id: match ? match[1] : null,
                        url: href,
                        title: cells[1]?.innerText.trim() || null,
                        updated_at: cells[2]?.innerText.trim() || null,
                        status_icon: icon?.getAttribute('data-name') || icon?.getAttribute('data-type') || null,
                status_color: statusContainer?.className.match(/(?:^|\s)(bg-[^\s]+)/)?.[1] || null
                    };
                });
            const total_match = document.body.innerText.match(/\bav\s+(\d+)\b/i);
            const page_size = document.querySelector('input[role="combobox"]')?.value;
            return {
                page_size: page_size ? Number(page_size) : null,
                total: total_match ? Number(total_match[1]) : null,
                rows
            };
        }"#,
    )
    .await?
    .into_value()
    .map_err(Into::into)
}

fn documentation_table_is_rendered(readiness: &DocumentationTableReadiness) -> bool {
    if !readiness.has_table || readiness.linked_rows > 0 {
        return readiness.has_table;
    }

    let lower = readiness.body_text.to_lowercase();
    let has_total = lower
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|pair| pair[0] == "av" && pair[1].parse::<u32>().is_ok());
    let has_empty_state = ["ingen data", "ingen dokumentasjoner", "ingen treff"]
        .iter()
        .any(|marker| lower.contains(marker));

    has_total || has_empty_state
}

async fn wait_for_documentation_rows(page: &chromiumoxide::Page) -> Result<()> {
    for _ in 0..20 {
        let readiness: DocumentationTableReadiness = page
            .evaluate(
                "() => ({ has_table: !!document.querySelector('table'), linked_rows: document.querySelectorAll('table a[href^=\"/l/dokumentasjon/\"]').length, body_text: document.body?.innerText || '' })",
            )
            .await?
            .into_value()?;
        if documentation_table_is_rendered(&readiness) {
            return Ok(());
        }
        sleep(Duration::from_millis(250)).await;
    }
    bail!("documentation table did not finish loading")
}

async fn documentation_detail_snapshot(
    page: &chromiumoxide::Page,
) -> Result<DocumentationDetailSnapshot> {
    page.evaluate(
        r#"() => {
            const main = document.querySelector('main');
            const knownStatuses = ['Kladd', 'Godkjent', 'Må rettes', 'Trenger endring', 'Til vurdering', 'Sendt', 'Avvist'];
            // The top status card is distinct from documentation text and feedback.
            // If its structure changes, return unknown instead of guessing.
            const statusCards = Array.from(main?.querySelectorAll('.vcard.bg-dark') || [])
                .filter(card => !card.closest('.ql-editor') && card.querySelector('i[data-name], i[data-type]'));
            const statusParagraphs = statusCards.flatMap(card => Array.from(card.querySelectorAll('p.text-lg.font-medium')))
                .filter(element => knownStatuses.includes(element.innerText.trim()));
            const statusParagraph = statusParagraphs.length === 1 ? statusParagraphs[0] : null;
            const statusIcon = Array.from(main?.querySelectorAll('i[data-name], i[data-type]') || [])
                .find(element => ['check-circle', 'edit', 'edit-2', 'clock'].includes(element.getAttribute('data-name') || element.getAttribute('data-type')));
            const colorContainer = statusIcon?.closest('[class*="bg-"]');
            const updated = Array.from(main?.querySelectorAll('small') || [])
                .find(element => element.innerText.trim().startsWith('Sist endret:'));
            const editors = Array.from(main?.querySelectorAll('.ql-editor') || []);
            const content = editors.find(element => !element.classList.contains('fb-editor')) || editors[0];
            const attachmentButton = Array.from(main?.querySelectorAll('button') || [])
                .find(button => /^Vedlegg \(\d+\)$/.test(button.innerText.trim()));
            const attachmentPanel = attachmentButton?.closest('.FExpantion');
            const attachmentMetadata = root => Array.from(root?.querySelectorAll('.FFileViewerBtn') || [])
                .map((element, index) => {
                    const clickable = element.matches('a,button') ? element : element.querySelector('a,button') || element.closest('a,button');
                    const rawUrl = clickable?.getAttribute('href') || clickable?.getAttribute('data-url') || element.getAttribute('data-url');
                    let url = null;
                    try {
                        const parsed = rawUrl ? new URL(rawUrl, window.location.href) : null;
                        url = parsed && (parsed.protocol === 'http:' || parsed.protocol === 'https:') ? parsed.href : null;
                    } catch (_) {}
                    const name = (element.querySelector('.FFileViewerBtn-content span')?.innerText || clickable?.getAttribute('aria-label') || element.getAttribute('title') || element.innerText || '').trim() || null;
                    const mime = clickable?.getAttribute('data-mime-type') || clickable?.getAttribute('data-mime') || element.getAttribute('data-mime-type') || null;
                    const sizeValue = clickable?.getAttribute('data-size-bytes') || element.getAttribute('data-size-bytes');
                    const parsedSize = sizeValue && /^\d+$/.test(sizeValue) ? Number(sizeValue) : null;
                    return {ordinal: index + 1, name, url, mime_type: mime, size_bytes: parsedSize};
                });
            const attachments = attachmentMetadata(attachmentPanel);
            const targetButton = Array.from(main?.querySelectorAll('button') || [])
                .find(button => button.innerText.trim().startsWith('Se kompetansemål'));
            const targetPanel = targetButton?.closest('.FExpantion');
            const feedbackCard = Array.from(main?.querySelectorAll('.vcard') || [])
                .find(card => Array.from(card.querySelectorAll('h3')).some(heading => heading.innerText.trim() === 'Tilbakemeldinger'));
            const feedback = Array.from(feedbackCard?.querySelectorAll('.divide-y > div') || [])
                .map(item => {
                    const badge = item.querySelector('.FBadge');
                    const icon = badge?.querySelector('i[data-name], i[data-type]');
                    return {
                        status_label: badge?.innerText.trim() || null,
                        status_icon: icon?.getAttribute('data-name') || icon?.getAttribute('data-type') || null,
                        status_color: null,
                        author: item.querySelector('span')?.innerText.trim() || null,
                        timestamp: item.querySelector('small')?.innerText.trim() || null,
                        text: item.querySelector('p')?.innerText.trim() || '',
                        attachments: attachmentMetadata(item)
                    };
                })
            return {
                title: main?.querySelector('h3')?.innerText.trim() || null,
                status_label: statusParagraph?.innerText.trim() || null,
                status_icon: statusIcon?.getAttribute('data-name') || statusIcon?.getAttribute('data-type') || null,
                status_color: colorContainer?.className.match(/(?:^|\s)(bg-[^\s]+)/)?.[1] || null,
                updated_at: updated?.innerText.replace(/^Sist endret:\s*/, '').trim() || null,
                content_text: content?.innerText.trim() || null,
                content_html: content?.innerHTML || null,
                attachments,
                target_text: targetPanel?.innerText.trim() || null,
                feedback
            };
        }"#,
    )
    .await?
    .into_value()
    .map_err(Into::into)
}

async fn wait_for_feedback_section(page: &chromiumoxide::Page) -> Result<()> {
    for _ in 0..20 {
        let rendered: bool = page
            .evaluate(
                "() => { const card = Array.from(document.querySelectorAll('main .vcard')).find(element => Array.from(element.querySelectorAll('h3')).some(heading => heading.innerText.trim() === 'Tilbakemeldinger')); if (!card) return false; return !!card.querySelector('.divide-y > div') || (card.innerText || '').includes('Ingen tilbakemeldinger'); }",
            )
            .await?
            .into_value()?;
        if rendered {
            return Ok(());
        }
        sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}

fn documentation_list_url(page_number: u32, page_size: u32, query: Option<&str>) -> String {
    let query = query.map(str::trim).unwrap_or_default();
    let mut encoded = String::new();
    for byte in query.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    format!("{DOCUMENTATION_URL}?size={page_size}&p={page_number}&q={encoded}")
}

pub async fn list_documentation(
    page_number: u32,
    page_size: u32,
    query: Option<String>,
    status: Option<DocumentationStatus>,
) -> Result<DocumentationPage> {
    if page_number == 0 {
        bail!("page must be at least 1");
    }
    if !DOCUMENTATION_PAGE_SIZES.contains(&page_size) {
        bail!("page_size must be one of 6, 12, 18, 24, 100, or 200");
    }

    // The table keeps its last search/page size/page in localStorage. Always
    // pass all three in the URL so a previous query cannot leak into this read.
    let url = documentation_list_url(page_number, page_size, query.as_deref());
    let (browser, handler_task, page, owns_browser) = open_page(&url).await?;
    wait_for_text(&page, "En oversikt over alle dine dokumentasjoner").await?;
    wait_for_documentation_rows(&page).await?;

    if let Some(status) = status {
        let label = match status {
            DocumentationStatus::Draft => "Kladd",
            DocumentationStatus::NeedsCorrection => "Må rettes",
            DocumentationStatus::Approved => "Godkjent",
            DocumentationStatus::Submitted | DocumentationStatus::InReview => "Til vurdering",
            DocumentationStatus::Rejected => "Avvist",
            DocumentationStatus::Unknown => "Alle",
        };
        let opened = page
            .evaluate(
                "() => { const button = Array.from(document.querySelectorAll('button')).find(button => (button.textContent || '').includes('Vis') && (button.textContent || '').includes('Alle')); if (!button) return false; button.click(); return true; }",
            )
            .await?
            .into_value::<bool>()?;
        if !opened {
            finish(browser, handler_task, owns_browser).await?;
            bail!("could not open the documentation status filter");
        }
        let label = serde_json::to_string(label)?;
        let selected = page
            .evaluate(format!(
                "() => {{ const item = Array.from(document.querySelectorAll('.q-menu *')).find(element => (element.textContent || '').trim().startsWith({label})); if (!item) return false; item.click(); return true; }}"
            ))
            .await?
            .into_value::<bool>()?;
        if !selected {
            finish(browser, handler_task, owns_browser).await?;
            bail!("documentation status filter is not exposed by the current UI");
        }
        sleep(Duration::from_millis(500)).await;
        wait_for_documentation_rows(&page).await?;
    }

    let snapshot = documentation_list_snapshot(&page).await?;
    let actual_page_size = snapshot.page_size.unwrap_or(page_size);
    let items = parse_documentation_rows(snapshot.rows);
    let result = DocumentationPage {
        page: page_number,
        page_size: actual_page_size,
        total: snapshot.total,
        items,
    };
    finish(browser, handler_task, owns_browser).await?;
    Ok(result)
}

pub async fn get_documentation(document_id: &str) -> Result<DocumentationRecord> {
    validate_documentation_id(document_id)?;

    let url = format!("{DOCUMENTATION_URL}/{document_id}");
    let (browser, handler_task, page, owns_browser) = open_page(&url).await?;
    wait_for_text(&page, "Sist endret:").await?;
    wait_for_feedback_section(&page).await?;
    let _ = click_documentation_expansion(&page, "Vedlegg (").await?;
    let _ = click_documentation_expansion(&page, "Se kompetansemål").await?;
    sleep(Duration::from_millis(250)).await;
    let actual_url = page.url().await?.unwrap_or_default();
    if actual_url.trim_end_matches('/') != url {
        finish(browser, handler_task, owns_browser).await?;
        bail!(
            "requested document {document_id}, but the visible page has a different URL: {actual_url}"
        );
    }
    let snapshot = documentation_detail_snapshot(&page).await?;
    let result = documentation_record_from_detail(document_id.to_string(), actual_url, snapshot);
    finish(browser, handler_task, owns_browser).await?;
    Ok(result)
}

fn validate_documentation_id(document_id: &str) -> Result<()> {
    if document_id.is_empty()
        || !document_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("document_id must be the ID from a Fagbrev documentation link");
    }
    Ok(())
}

/// Validate a target before a write is allowed to reach the browser UI.
///
/// Delmål are UI-local references in the current site, so a write requires
/// both the parent goal number and the visible ordinal/title. No backend ID is
/// inferred here.
pub fn validate_documentation_target(target: &DocumentationTarget) -> Result<()> {
    if target.competency_goal.is_none() && target.delmal.is_none() {
        bail!("target must include a competency_goal, a delmal, or both");
    }

    if let Some(goal) = &target.competency_goal {
        if !(1..=21).contains(&goal.number) {
            bail!("competency_goal.number must be between 1 and 21");
        }
        if goal
            .title
            .as_deref()
            .is_some_and(|title| title.trim().is_empty())
        {
            bail!("competency_goal.title cannot be empty when provided");
        }
    }

    if let Some(delmal) = &target.delmal {
        if !(1..=21).contains(&delmal.goal_number) {
            bail!("delmal.goal_number must be between 1 and 21");
        }
        if delmal.number.is_none_or(|number| number == 0) {
            bail!("delmal.number is required and must be greater than zero");
        }
        if delmal.title.trim().is_empty() {
            bail!("delmal.title cannot be empty");
        }
        if target
            .competency_goal
            .as_ref()
            .is_some_and(|goal| goal.number != delmal.goal_number)
        {
            bail!("competency_goal.number must match delmal.goal_number");
        }
    }

    Ok(())
}

/// Compare the saved UI text conservatively while tolerating HTML whitespace.
fn comparable_document_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn verify_saved_documentation(
    record: &DocumentationRecord,
    document_id: &str,
    title: &str,
    content: &str,
    target: &DocumentationTarget,
) -> Result<()> {
    if record.id.as_deref() != Some(document_id)
        || record
            .url
            .as_deref()
            .and_then(documentation_id_from_url)
            .as_deref()
            != Some(document_id)
    {
        bail!("saved document identity could not be verified");
    }
    if record
        .title
        .as_deref()
        .map(comparable_document_text)
        .as_deref()
        != Some(comparable_document_text(title).as_str())
    {
        bail!("saved title differs from the expected title");
    }
    let expected_content = comparable_document_text(content);
    if expected_content.is_empty()
        || record
            .content
            .as_deref()
            .map(comparable_document_text)
            .as_deref()
            != Some(expected_content.as_str())
    {
        bail!("saved content is empty, unavailable, or differs from the expected content");
    }
    let summary = record
        .target_summary
        .as_ref()
        .context("saved target selection could not be read")?;
    if target.delmal.is_some() {
        // The detail page shows selected titles, not their ordinal in the full
        // plan. Do not claim that an ordinal has been verified by guessing.
        bail!(
            "saved delmål ordinal cannot be verified from the detail page; inspect this document manually"
        );
    }
    if let Some(goal) = &target.competency_goal {
        if summary.competency_goals.len() != 1
            || !summary.delmal.is_empty()
            || summary.competency_goals[0].number != goal.number
        {
            bail!("saved target selection differs from the expected competency goal");
        }
        if let Some(expected_title) = &goal.title
            && summary.competency_goals[0]
                .title
                .as_deref()
                .map(comparable_document_text)
                .as_deref()
                != Some(comparable_document_text(expected_title).as_str())
        {
            bail!("saved competency goal title differs from the expected title");
        }
    }
    if target.competency_goal.is_none() && target.delmal.is_none() {
        bail!("an expected target is required to verify the saved document");
    }
    Ok(())
}

fn documentation_id_from_url(url: &str) -> Option<String> {
    let id = url.split("/l/dokumentasjon/").nth(1)?.split('/').next()?;
    (!id.is_empty()
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_')))
    .then(|| id.to_string())
}

async fn fill_new_documentation_form(
    page: &chromiumoxide::Page,
    title: &str,
    content: &str,
    target: &DocumentationTarget,
) -> Result<()> {
    fill_documentation_form(page, title, content, Some(target), false).await
}

/// Fill the visible title/editor fields and, when requested, replace the
/// currently selected competency-goal/delmål checkboxes.
///
/// `replacement_target = None` deliberately leaves the existing selection
/// untouched. This is used by updates that only change title/content.
async fn fill_documentation_form(
    page: &chromiumoxide::Page,
    title: &str,
    content: &str,
    replacement_target: Option<&DocumentationTarget>,
    replace_targets: bool,
) -> Result<()> {
    let title = serde_json::to_string(title)?;
    let expected_title = title.clone();
    let mut targets = Vec::new();
    if let Some(target) = replacement_target {
        if let Some(goal) = &target.competency_goal {
            targets.push(serde_json::json!({
                "kind": "competency_goal",
                "number": goal.number,
                "title": goal.title,
            }));
        }
        if let Some(delmal) = &target.delmal {
            targets.push(serde_json::json!({
                "kind": "delmal",
                "goal_number": delmal.goal_number,
                "number": delmal.number,
                "title": delmal.title,
            }));
        }
    }
    let targets = serde_json::to_string(&targets)?;
    let replace_targets = serde_json::to_string(&replace_targets)?;

    let result: UiMutationResult = page
        .evaluate(format!(
            r#"() => {{
                const targets = {targets};
                const replaceTargets = {replace_targets};
                const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
                const comparable = value => normalize(value).toLocaleLowerCase('nb').replace(/[.:]+$/, '');
                const labels = Array.from(document.querySelectorAll('main label'));
                const found = targets.map(target => ({{ target, label: null }}));
                let currentGoal = null;
                let delmalOrdinal = 0;

                for (const label of labels) {{
                    const text = normalize(label.innerText);
                    const checkbox = label.querySelector('.FCheckbox');
                    const goalMatch = text.match(/^(\d+)\.\s+/);
                    if (checkbox && goalMatch) {{
                        currentGoal = Number(goalMatch[1]);
                        delmalOrdinal = 0;
                        const goalText = comparable(label.querySelector('.FCheckbox span')?.innerText || text);
                        for (const item of found.filter(item => item.target.kind === 'competency_goal')) {{
                            const titleMatches = !item.target.title || goalText.startsWith(comparable(`${{item.target.number}}. ${{item.target.title}}`));
                            if (currentGoal === item.target.number && titleMatches) item.label = label;
                        }}
                        continue;
                    }}
                    if (!checkbox || currentGoal === null) continue;
                    delmalOrdinal += 1;
                    for (const item of found.filter(item => item.target.kind === 'delmal')) {{
                        const titleMatches = !item.target.title || comparable(text) === comparable(item.target.title);
                        if (currentGoal === item.target.goal_number && delmalOrdinal === item.target.number && titleMatches) item.label = label;
                    }}
                }}

                const missing = found.filter(item => !item.label).map(item => item.target.kind);
                if (missing.length) {{
                    const goals = labels.filter(label => label.querySelector('.FCheckbox') && /^\d+\.\s/.test(normalize(label.innerText))).length;
                    return {{ok: false, missing, message: `one or more target references were not found in the visible picker (${{goals}} goal labels, ${{labels.length}} labels)`}};
                }}

                const input = document.querySelector('input[placeholder="Dokumentasjonavn"]');
                const editor = document.querySelector('main .ql-editor');
                if (!input || !editor) return {{ok: false, missing: [], message: 'the documentation title or editor field was not exposed by the UI'}};

                const selected = new Set(found.map(item => item.label.querySelector('.FCheckbox[role="checkbox"]')));
                if (selected.has(null)) return {{ok: false, missing: [], message: 'a target checkbox was not available'}};
                const controls = Array.from(document.querySelectorAll('main label .FCheckbox[role="checkbox"]'));
                for (const control of controls) {{
                    const checked = control.getAttribute('aria-checked');
                    if (checked !== 'true' && checked !== 'false') return {{ok: false, missing: [], message: 'target checkbox state is unknown'}};
                    const shouldSelect = selected.has(control);
                    if ((shouldSelect && checked === 'false') || (replaceTargets && !shouldSelect && checked === 'true')) control.click();
                }}
                return {{ok: true, missing: [], message: null}};
            }}"#
        ))
        .await?
        .into_value()?;

    if !result.ok {
        let mut detail = result
            .message
            .unwrap_or_else(|| "the documentation form could not be prepared".to_string());
        if !result.missing.is_empty() {
            detail.push_str(&format!(" (missing: {})", result.missing.join(", ")));
        }
        bail!(detail);
    }
    let title_ready: bool = page.evaluate("() => { const input = document.querySelector('input[placeholder=\"Dokumentasjonavn\"]'); if (!input) return false; input.focus(); input.select(); return true; }").await?.into_value()?;
    if !title_ready {
        bail!("title field unavailable; save was not attempted");
    }
    page.execute(InsertTextParams::new(serde_json::from_str::<String>(
        &expected_title,
    )?))
    .await?;
    let editor_ready: bool = page.evaluate("() => { const editor = document.querySelector('main .ql-editor'); if (!editor) return false; editor.focus(); const range = document.createRange(); range.selectNodeContents(editor); const selection = window.getSelection(); selection.removeAllRanges(); selection.addRange(range); return true; }").await?.into_value()?;
    if !editor_ready {
        bail!("editor unavailable; save was not attempted");
    }
    page.execute(InsertTextParams::new(content.to_string()))
        .await?;
    let expected_content = serde_json::to_string(&comparable_document_text(content))?;
    let filled: bool = page.evaluate(format!(r#"() => {{ const normalize = s => (s || '').replace(/\s+/g, ' ').trim(); return document.querySelector('input[placeholder="Dokumentasjonavn"]')?.value === {expected_title} && normalize(document.querySelector('main .ql-editor')?.innerText) === {expected_content}; }}"#)).await?.into_value()?;
    if !filled {
        bail!("filled title or content did not match; save was not attempted");
    }
    if let Some(target) = replacement_target {
        let expected = target.competency_goal.as_ref().map(|goal| goal.number);
        let script = format!(
            r#"() => {{
            const expected = {};
            const selected = Array.from(document.querySelectorAll('main label .FCheckbox[role="checkbox"][aria-checked="true"]'));
            return selected.length === 1 && selected[0].closest('label')?.innerText.trim().startsWith(`${{expected}}. `);
        }}"#,
            serde_json::to_string(&expected)?
        );
        let mut verified = false;
        for _ in 0..12 {
            if page.evaluate(script.clone()).await?.into_value::<bool>()? {
                verified = true;
                break;
            }
            sleep(Duration::from_millis(250)).await;
        }
        if !verified {
            bail!("target checkbox selection was not confirmed; save was not attempted");
        }
    }
    Ok(())
}

/// Create/save a documentation entry through the visible Fagbrev form.
///
/// This function is intentionally only called by the MCP layer after
/// `confirm: true`. Opening this form itself may create a blank draft.
pub async fn submit_documentation(
    title: &str,
    content: &str,
    target: &DocumentationTarget,
    confirm: bool,
) -> Result<DocumentationRecord> {
    if !confirm {
        bail!("submit_documentation requires confirm=true before opening the form");
    }
    validate_documentation_target(target)?;
    if target.delmal.is_some() {
        bail!("delmål selection cannot yet be verified safely; no document was created");
    }
    if title.trim().is_empty() {
        bail!("title cannot be empty");
    }
    if content.trim().is_empty() {
        bail!("content cannot be empty");
    }

    let _write_turn = DOCUMENTATION_WRITE_TURN.lock().await;
    let ((browser, handler_task, page, owns_browser), draft_id) =
        documentation_picker_page().await?;
    let result = async {
        let id = draft_id
            .as_deref()
            .context("draft ID was not visible; do not create another document")?;
        fill_new_documentation_form(&page, title, content, target).await?;
        click_save(&page).await?;
        let url = wait_for_saved_url(&page).await?;
        if url.trim_end_matches('/') != format!("{DOCUMENTATION_URL}/{id}") {
            bail!("save navigated to an unexpected document URL");
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let close_result = finish(browser, handler_task, owns_browser).await;
    let id = draft_id.as_deref();
    result.map_err(|error| with_blank_draft_hint(error, id))?;
    close_result.map_err(|error| with_blank_draft_hint(error, id))?;
    let id = id.context("draft ID is unknown; check the documentation list before trying again")?;
    let record = get_documentation(id)
        .await
        .map_err(|error| with_blank_draft_hint(error, Some(id)))?;
    verify_saved_documentation(&record, id, title, content, target)
        .map_err(|error| with_blank_draft_hint(error, Some(id)))?;
    Ok(record)
}

async fn click_save(page: &chromiumoxide::Page) -> Result<()> {
    let clicked: bool = page
        .evaluate(
            "() => { const button = Array.from(document.querySelectorAll('main button')).find(button => button.type === 'submit' && (button.textContent || '').trim() === 'Lagre'); if (!button) return false; button.click(); return true; }",
        )
        .await?
        .into_value()?;
    if !clicked {
        bail!("the documentation save control was not exposed by the UI");
    }
    Ok(())
}

/// Saving must route from `/{id}/rediger` back to the record.
async fn wait_for_saved_url(page: &chromiumoxide::Page) -> Result<String> {
    let mut url = String::new();
    for _ in 0..20 {
        sleep(Duration::from_millis(250)).await;
        url = page.url().await?.unwrap_or_default();
        if !url.trim_end_matches('/').ends_with("/rediger") {
            break;
        }
    }
    if url.is_empty() || url.trim_end_matches('/').ends_with("/rediger") {
        bail!("save was not confirmed: the editor did not navigate to the document");
    }
    Ok(url)
}

async fn wait_for_edit_form(page: &chromiumoxide::Page) -> Result<()> {
    for _ in 0..20 {
        let ready: bool = page
            .evaluate(
                "() => !!document.querySelector('input[placeholder=\"Dokumentasjonavn\"]') && !!document.querySelector('main .ql-editor') && Array.from(document.querySelectorAll('main button')).some(button => button.type === 'submit' && (button.textContent || '').trim() === 'Lagre')",
            )
            .await?
            .into_value()?;
        if ready {
            return Ok(());
        }
        sleep(Duration::from_millis(250)).await;
    }
    bail!("the documentation edit form did not finish loading")
}

/// Edit an existing documentation record through its visible editor.
///
/// The caller must provide the exact last-seen timestamp. The detail page is
/// read first after confirmation and the edit page is only opened if that
/// optimistic concurrency check still matches.
pub async fn update_documentation(
    document_id: &str,
    title: &str,
    content: &str,
    replacement_target: Option<&DocumentationTarget>,
    expected_updated_at: &str,
    confirm: bool,
) -> Result<DocumentationRecord> {
    validate_documentation_id(document_id)?;
    if !confirm {
        bail!("update_documentation requires confirm=true before opening the edit form");
    }
    if title.trim().is_empty() {
        bail!("title cannot be empty");
    }
    if content.trim().is_empty() {
        bail!("content cannot be empty");
    }
    if expected_updated_at.trim().is_empty() {
        bail!("expected_updated_at is required for safe documentation updates");
    }
    if let Some(target) = replacement_target {
        validate_documentation_target(target)?;
        if target.delmal.is_some() {
            bail!(
                "document {document_id}: delmål selection cannot yet be verified safely; nothing was changed"
            );
        }
    }

    let _write_turn = DOCUMENTATION_WRITE_TURN.lock().await;
    let current = get_documentation(document_id).await?;
    if replacement_target.is_none() {
        let summary = current
            .target_summary
            .as_ref()
            .context("existing target cannot be verified; nothing was changed")?;
        if summary.competency_goals.len() != 1 || !summary.delmal.is_empty() {
            bail!(
                "document {document_id}: existing target cannot be verified safely; nothing was changed"
            );
        }
    }
    let current_updated_at = current
        .updated_at
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("the documentation has no visible updated timestamp"))?;
    if current_updated_at != expected_updated_at {
        bail!(
            "documentation changed since it was read: expected updated_at {:?}, current value is {:?}",
            expected_updated_at,
            current_updated_at
        );
    }

    let url = format!("{DOCUMENTATION_URL}/{document_id}/rediger");
    let (browser, handler_task, page, owns_browser) = open_page(&url).await?;
    let result = async {
        wait_for_edit_form(&page).await?;
        if replacement_target.is_some() {
            wait_for_target_picker(&page).await?;
        }
        fill_documentation_form(
            &page,
            title,
            content,
            replacement_target,
            replacement_target.is_some(),
        )
        .await?;
        click_save(&page).await?;
        wait_for_saved_url(&page).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let close_result = finish(browser, handler_task, owns_browser).await;
    result.with_context(|| {
        format!("document {document_id} may have been modified; inspect this ID before retrying")
    })?;
    close_result.with_context(|| {
        format!("document {document_id} may have been modified; inspect this ID before retrying")
    })?;

    let record = get_documentation(document_id).await.with_context(|| format!(
        "update of document {document_id} may have been applied, but could not be verified; inspect this ID before retrying"
    ))?;
    let expected_target = if let Some(target) = replacement_target {
        target.clone()
    } else {
        let summary = current.target_summary.as_ref().with_context(|| format!(
            "document {document_id}: previous target could not be read; update cannot be verified"
        ))?;
        DocumentationTarget {
            competency_goal: summary.competency_goals.first().cloned(),
            delmal: summary.delmal.first().cloned(),
        }
    };
    verify_saved_documentation(&record, document_id, title, content, &expected_target)
        .with_context(|| format!("update of document {document_id} may have been applied, but verification failed; inspect this ID before retrying"))?;
    if replacement_target.is_none() && current.target_summary != record.target_summary {
        bail!(
            "update of document {document_id} may have changed target selection; inspect this ID before retrying"
        );
    }
    Ok(record)
}

/// Attempt to delete a draft only when the visible detail page exposes a
/// clearly labelled delete control. Current Fagbrev pages do not expose one,
/// so the normal confirmed result is an explicit unsupported response.
pub async fn delete_documentation(
    document_id: &str,
    expected_status: DocumentationStatus,
    expected_updated_at: &str,
    confirm: bool,
) -> Result<DeleteDocumentationResult> {
    validate_documentation_id(document_id)?;
    if expected_status != DocumentationStatus::Draft {
        bail!("delete_documentation only permits expected_status=draft");
    }
    if expected_updated_at.trim().is_empty() {
        bail!("expected_updated_at is required for safe documentation deletion");
    }
    if !confirm {
        bail!("delete_documentation requires confirm=true before opening a documentation page");
    }

    let _write_turn = DOCUMENTATION_WRITE_TURN.lock().await;
    let current = get_documentation(document_id).await?;
    if current.status != DocumentationStatus::Draft {
        bail!("refusing to delete a documentation record that is not a draft");
    }
    let current_updated_at = current
        .updated_at
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("the documentation has no visible updated timestamp"))?;
    if current_updated_at != expected_updated_at {
        bail!(
            "documentation changed since it was read: expected updated_at {:?}, current value is {:?}",
            expected_updated_at,
            current_updated_at
        );
    }

    let url = format!("{DOCUMENTATION_URL}/{document_id}");
    let (browser, handler_task, page, owns_browser) = open_page(&url).await?;
    wait_for_text(&page, "Sist endret:").await?;
    let result = async {
        let delete_visible: bool = page
            .evaluate(
                "() => Array.from(document.querySelectorAll('main button, main a')).some(element => /^(Slett|Slett dokumentasjon)$/.test((element.textContent || '').replace(/\\s+/g, ' ').trim()))",
            )
            .await?
            .into_value()?;
        if !delete_visible {
            return Ok(delete_unsupported_result(
                document_id,
                expected_status,
                expected_updated_at,
                current.status,
            ));
        }

        let clicked: bool = page
            .evaluate(
                "() => { const control = Array.from(document.querySelectorAll('main button, main a')).find(element => /^(Slett|Slett dokumentasjon)$/.test((element.textContent || '').replace(/\\s+/g, ' ').trim())); if (!control) return false; control.click(); return true; }",
            )
            .await?
            .into_value()?;
        if !clicked {
            bail!("the visible delete control could not be activated");
        }
        sleep(Duration::from_millis(750)).await;
        Ok(DeleteDocumentationResult {
            outcome: "deleted".to_string(),
            confirmation_required: false,
            would_mutate: true,
            mutated: true,
            document_id: document_id.to_string(),
            expected_status,
            expected_updated_at: expected_updated_at.to_string(),
            status: Some(DocumentationStatus::Draft),
            message: "The clearly labelled visible delete action was activated.".to_string(),
        })
    }
    .await;
    finish(browser, handler_task, owns_browser).await?;
    result
}

fn delete_unsupported_result(
    document_id: &str,
    expected_status: DocumentationStatus,
    expected_updated_at: &str,
    current_status: DocumentationStatus,
) -> DeleteDocumentationResult {
    DeleteDocumentationResult {
        outcome: "unsupported".to_string(),
        confirmation_required: false,
        would_mutate: false,
        mutated: false,
        document_id: document_id.to_string(),
        expected_status,
        expected_updated_at: expected_updated_at.to_string(),
        status: Some(current_status),
        message: "The current documentation page does not expose a clearly labelled delete action; nothing was changed.".to_string(),
    }
}

/// Request approval using the visible `Send inn` action on a documentation
/// detail page. If that action is absent, no click is attempted.
pub async fn request_approval(
    document_id: &str,
    title: &str,
    content: &str,
    target: &DocumentationTarget,
    confirm: bool,
) -> Result<RequestApprovalResult> {
    if !confirm {
        bail!("request_approval requires confirm=true before opening a documentation page");
    }
    validate_documentation_id(document_id)?;
    validate_documentation_target(target)?;
    if target.delmal.is_some() {
        bail!(
            "document {document_id}: delmål selection cannot yet be verified safely; nothing was submitted"
        );
    }
    if title.trim().is_empty() || content.trim().is_empty() {
        bail!(
            "document {document_id}: expected title and content must not be empty; nothing was submitted"
        );
    }

    let _write_turn = DOCUMENTATION_WRITE_TURN.lock().await;
    let url = format!("{DOCUMENTATION_URL}/{document_id}");
    let (browser, handler_task, page, owns_browser) = open_page(&url).await?;
    let result = async {
        let snapshot = wait_for_text(&page, "Sist endret:").await?;
        if snapshot.url.trim_end_matches('/') != url || !snapshot.text.contains("Sist endret:") {
            bail!("document {document_id}: detail page identity or loading could not be verified; nothing was submitted");
        }
        if !click_documentation_expansion(&page, "Se kompetansemål").await? {
            bail!("document {document_id}: target panel was not available; nothing was submitted");
        }
        sleep(Duration::from_millis(250)).await;
        let detail = documentation_detail_snapshot(&page).await?;
        let record = documentation_record_from_detail(document_id.to_string(), snapshot.url, detail);
        verify_saved_documentation(&record, document_id, title, content, target)
            .with_context(|| format!("document {document_id}: saved data could not be verified; nothing was submitted"))?;
        if !matches!(record.status, DocumentationStatus::Draft | DocumentationStatus::NeedsCorrection) {
            bail!("document {document_id}: current status is not a confirmed editable draft; nothing was submitted");
        }
        let clicked: bool = page
            .evaluate(
                "() => { const button = Array.from(document.querySelectorAll('main button')).find(button => (button.textContent || '').trim() === 'Send inn'); if (!button) return false; button.click(); return true; }",
            )
            .await
            .with_context(|| format!("document {document_id}: submission may have been activated; inspect this ID before retrying"))?
            .into_value()
            .with_context(|| format!("document {document_id}: submission may have been activated; inspect this ID before retrying"))?;
        if !clicked {
            return Ok(RequestApprovalResult {
                outcome: "unsupported".to_string(),
                confirmation_required: false,
                would_mutate: false,
                mutated: false,
                document_id: document_id.to_string(),
                url: Some(url.clone()),
                status: None,
                message: "The current documentation page does not expose a safe 'Send inn' approval action; nothing was changed.".to_string(),
            });
        }

        // Keep the page alive while the site's asynchronous submission starts.
        sleep(Duration::from_secs(2)).await;
        Ok(RequestApprovalResult {
            outcome: "unconfirmed".to_string(),
            confirmation_required: false,
            would_mutate: true,
            mutated: true,
            document_id: document_id.to_string(),
            url: Some(url.clone()),
            status: None,
            message: "The visible 'Send inn' action was activated; submission has not yet been confirmed. Inspect this document ID before retrying.".to_string(),
        })
    }
    .await;
    let close_result = finish(browser, handler_task, owns_browser).await;
    let mut result = result
        .with_context(|| format!("document {document_id}: inspect this ID before retrying"))?;
    if let Err(error) = close_result {
        if result.mutated {
            result.message = format!(
                "Submission may have been activated for document {document_id}, but the browser could not be closed: {error:#}. Inspect this ID before retrying."
            );
            return Ok(result);
        }
        return Err(error);
    }
    if !result.mutated {
        return Ok(result);
    }
    // Open a fresh page after releasing the shared-browser lock. A click alone
    // is never evidence that the server accepted the submission.
    for _ in 0..4 {
        sleep(Duration::from_millis(750)).await;
        if let Ok(record) = get_documentation(document_id).await
            && matches!(
                record.status,
                DocumentationStatus::Submitted | DocumentationStatus::InReview
            )
            && verify_saved_documentation(&record, document_id, title, content, target).is_ok()
        {
            result.outcome = "requested".to_string();
            result.status = Some(record.status);
            result.message = "Submission status and saved document contents were confirmed on a fresh detail page.".to_string();
            return Ok(result);
        }
    }
    Ok(result)
}

pub async fn dashboard_overview() -> Result<DashboardOverview> {
    let (browser, handler_task, page, owns_browser) = open_dashboard(false).await?;
    if !page
        .url()
        .await?
        .unwrap_or_default()
        .trim_end_matches('/')
        .ends_with("fagbrev.io/l")
    {
        page.goto(FAGBREV_URL).await?;
        sleep(Duration::from_millis(750)).await;
    }
    let mut snapshot = page_snapshot(&page).await?;
    for _ in 0..15 {
        if is_authenticated(&snapshot) {
            break;
        }
        sleep(Duration::from_millis(500)).await;
        snapshot = page_snapshot(&page).await?;
    }
    let overview = dashboard_overview_from_snapshot(snapshot);
    finish(browser, handler_task, owns_browser).await?;
    Ok(overview)
}

pub async fn list_competency_goals() -> Result<Vec<CompetencyGoal>> {
    let (browser, handler_task, page, owns_browser) = plan_page(false).await?;
    let result = async {
        let mut snapshot = page_snapshot(&page).await?;
        let mut goals = parse_goals_from_page_text(&snapshot.text);
        for _ in 0..20 {
            if goal_list_is_complete(&goals) {
                return Ok(goals);
            }
            sleep(Duration::from_millis(250)).await;
            snapshot = page_snapshot(&page).await?;
            goals = parse_goals_from_page_text(&snapshot.text);
        }
        Err(goal_list_error(&goals, &snapshot))
    }
    .await;
    finish(browser, handler_task, owns_browser).await?;
    result
}

#[derive(Debug, Clone, Deserialize)]
struct ExpandedGoalSnapshot {
    number: u8,
    status_count: Option<u32>,
    ready: bool,
    description: Option<String>,
    delmal: Vec<DelmalSnapshot>,
}

#[derive(Debug, Clone, Deserialize)]
struct DelmalSnapshot {
    title: String,
    status_count: Option<u32>,
}

struct ExpandedGoal {
    title: Option<String>,
    status_count: Option<u32>,
    description: Option<String>,
    delmal: Vec<Delmal>,
}

/// Shared plan-page lookup: `goalRoots(numbers)` maps each requested goal
/// number to its expansion trigger and panel root.
const GOAL_ROOTS_JS: &str = r#"
    const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
    const goalRoots = numbers => {
        const roots = new Map();
        for (const button of document.querySelectorAll('main button.FExpantion-trigger')) {
            const heading = (button.innerText || '').split('\n').map(normalize).find(line => /^\d+\.\s/.test(line));
            const number = heading ? Number(heading.match(/^(\d+)\./)[1]) : null;
            if (numbers.includes(number) && !roots.has(number)) roots.set(number, { button, root: button.parentElement });
        }
        return roots;
    };
"#;

/// Expand the requested goals on the plan page in one pass and read their
/// delmål from the rendered "Delmål ( N )" list.
///
/// The list is read from the DOM because the goal description above it often
/// contains a bulleted "Forslag til oppgaver" list that is not the delmål.
async fn expand_goals(page: &chromiumoxide::Page, numbers: &[u8]) -> Result<Vec<ExpandedGoal>> {
    let wanted = serde_json::to_string(numbers)?;
    let found: Vec<u8> = page
        .evaluate(format!(
            r#"() => {{
                {GOAL_ROOTS_JS}
                const roots = goalRoots({wanted});
                for (const {{ button, root }} of roots.values()) {{
                    if (!root?.querySelector('.FExpantion-arrow-open')) button.click();
                }}
                return Array.from(roots.keys());
            }}"#
        ))
        .await?
        .into_value()?;
    if let Some(missing) = numbers.iter().find(|number| !found.contains(number)) {
        bail!("could not find competency goal {missing} on the plan page");
    }

    let snapshot_script = format!(
        r#"() => {{
            {GOAL_ROOTS_JS}
            const roots = goalRoots({wanted});
            return Array.from(roots.entries()).map(([number, {{ root }}]) => {{
                const content = root?.querySelector('.FExpantion-content');
                const delmal = Array.from(root?.querySelectorAll('[group="subGoals"]') || []).map(item => {{
                    const count = normalize(item.querySelector('small')?.innerText);
                    return {{ title: normalize(item.querySelector('p')?.innerText), status_count: /^\d+$/.test(count) ? Number(count) : null }};
                }}).filter(item => item.title);
                const countLine = (root?.querySelector('button.FExpantion-trigger')?.innerText || '').split('\n').map(normalize).find(line => /^\d+$/.test(line));
                const status_count = countLine ? Number(countLine) : null;
                // Structure renders before the per-goal/per-delmål counts arrive.
                const countsLoaded = status_count !== null && delmal.every(item => item.status_count !== null);
                return {{
                    number,
                    status_count,
                    ready: !!content && countsLoaded && (delmal.length > 0 || (root.innerText || '').includes('Se tilknyttede dokumentasjoner')),
                    description: content?.querySelector('.ql-editor')?.innerText?.trim() || null,
                    delmal
                }};
            }});
        }}"#
    );
    let mut snapshots: Vec<ExpandedGoalSnapshot> = Vec::new();
    for _ in 0..30 {
        snapshots = page
            .evaluate(snapshot_script.as_str())
            .await?
            .into_value()?;
        if snapshots.iter().all(|snapshot| snapshot.ready) {
            break;
        }
        sleep(Duration::from_millis(150)).await;
    }

    let text = page_snapshot(page).await?.text;
    Ok(numbers
        .iter()
        .map(|&number| {
            let (title, status_count, text_delmal) =
                parse_expanded_goal_from_page_text(&text, number);
            let snapshot = snapshots.iter().find(|snapshot| snapshot.number == number);
            let delmal = match snapshot {
                Some(snapshot) if snapshot.ready || !snapshot.delmal.is_empty() => snapshot
                    .delmal
                    .iter()
                    .enumerate()
                    .map(|(index, item)| Delmal {
                        goal_number: number,
                        number: Some(index as u16 + 1),
                        title: item.title.clone(),
                        id: None,
                        status: None,
                        status_count: item.status_count,
                    })
                    .collect(),
                _ => text_delmal,
            };
            ExpandedGoal {
                title,
                status_count: snapshot
                    .and_then(|snapshot| snapshot.status_count)
                    .or(status_count),
                description: snapshot.and_then(|snapshot| snapshot.description.clone()),
                delmal,
            }
        })
        .collect())
}

pub async fn competency_goal(number: u8) -> Result<CompetencyGoalDetails> {
    if !(1..=21).contains(&number) {
        bail!("goal number must be between 1 and 21");
    }

    let (browser, handler_task, page, owns_browser) = plan_page(false).await?;
    let result = async {
        let goal = expand_goals(&page, &[number])
            .await?
            .pop()
            .context("competency goal expansion returned no result")?;
        let attachments = competency_goal_attachment_snapshot(&page, number, None).await?;
        Ok(CompetencyGoalDetails {
            number,
            url: page.url().await?.unwrap_or_default(),
            page_title: page.get_title().await?.unwrap_or_default(),
            description: goal.description,
            title: goal.title,
            status_count: goal.status_count,
            delmal: goal.delmal,
            attachments,
        })
    }
    .await;
    finish(browser, handler_task, owns_browser).await?;
    result
}

async fn delmal_title(
    page: &chromiumoxide::Page,
    goal_number: u8,
    delmal_number: u16,
) -> Result<String> {
    let goal = expand_goals(page, &[goal_number])
        .await?
        .pop()
        .context("competency goal expansion returned no result")?;
    goal.delmal
        .into_iter()
        .find(|item| item.number == Some(delmal_number))
        .map(|item| item.title)
        .with_context(|| format!("could not find delmål {delmal_number} for goal {goal_number}"))
}

async fn delmal_attachments(
    goal_number: u8,
    delmal_number: u16,
) -> Result<Vec<AttachmentMetadata>> {
    let (browser, handler_task, page, owns_browser) = plan_page(false).await?;
    let result = async {
        let title = delmal_title(&page, goal_number, delmal_number).await?;
        competency_goal_attachment_snapshot(&page, goal_number, Some(&title)).await
    }
    .await;
    finish(browser, handler_task, owns_browser).await?;
    result
}

pub async fn list_delmal(goal_number: Option<u8>) -> Result<Vec<Delmal>> {
    if let Some(number) = goal_number
        && !(1..=21).contains(&number)
    {
        bail!("goal number must be between 1 and 21");
    }

    // The documentation target picker is not read-only: opening a new-entry
    // form has been observed to create a blank draft. Read delmål from the
    // existing training-plan UI instead, expanding every goal on one page.
    let goal_numbers = goal_number.map_or_else(|| (1..=21).collect(), |number| vec![number]);
    let (browser, handler_task, page, owns_browser) = plan_page(false).await?;
    let result = expand_goals(&page, &goal_numbers)
        .await
        .map(|goals| goals.into_iter().flat_map(|goal| goal.delmal).collect());
    finish(browser, handler_task, owns_browser).await?;
    result
}

pub async fn get_delmal(goal_number: u8, delmal_number: u16) -> Result<Delmal> {
    if !(1..=21).contains(&goal_number) {
        bail!("goal number must be between 1 and 21");
    }
    let delmal = list_delmal(Some(goal_number))
        .await?
        .into_iter()
        .find(|item| item.number == Some(delmal_number));
    delmal.ok_or_else(|| {
        anyhow::anyhow!("could not find delmål {delmal_number} for goal {goal_number}")
    })
}

pub async fn list_half_year_tasks() -> Result<Vec<HalfYearTask>> {
    let (browser, handler_task, page, owns_browser) = half_year_page(false).await?;
    let result = async {
        let snapshot = half_year_tasks_snapshot(&page).await?;
        Ok::<Vec<HalfYearTask>, anyhow::Error>(parse_half_year_tasks(snapshot))
    }
    .await;
    finish(browser, handler_task, owns_browser).await?;
    result
}

pub async fn get_half_year_task(ordinal: u8) -> Result<HalfYearTask> {
    if !(1..=6).contains(&ordinal) {
        bail!("half-year task ordinal must be between 1 and 6");
    }
    list_half_year_tasks()
        .await?
        .into_iter()
        .find(|task| task.ordinal == ordinal)
        .ok_or_else(|| anyhow::anyhow!("could not find half-year task {ordinal}"))
}

pub async fn list_feedback(document_id: &str) -> Result<Vec<FeedbackRecord>> {
    Ok(get_documentation(document_id).await?.feedback)
}

pub async fn get_feedback(document_id: &str, ordinal: u32) -> Result<FeedbackRecord> {
    if ordinal == 0 {
        bail!("feedback ordinal must be greater than zero");
    }
    list_feedback(document_id)
        .await?
        .into_iter()
        .find(|feedback| feedback.ordinal == ordinal)
        .ok_or_else(|| {
            anyhow::anyhow!("could not find feedback {ordinal} for document {document_id}")
        })
}

fn validate_attachment_source(source: &AttachmentSource) -> Result<()> {
    match source {
        AttachmentSource::Documentation { document_id }
        | AttachmentSource::Feedback { document_id, .. } => validate_documentation_id(document_id),
        AttachmentSource::CompetencyGoal { goal_number }
        | AttachmentSource::Delmal { goal_number, .. } => {
            if !(1..=21).contains(goal_number) {
                bail!("goal number must be between 1 and 21");
            }
            if let AttachmentSource::Delmal { delmal_number, .. } = source
                && *delmal_number == 0
            {
                bail!("delmal_number must be greater than zero");
            }
            Ok(())
        }
    }
}

fn attachment_list_result(
    source: AttachmentSource,
    attachments: Vec<AttachmentMetadata>,
) -> AttachmentList {
    let warnings = if attachments.is_empty() {
        vec![
            "No attachment controls were exposed by the current Fagbrev UI for this source."
                .to_string(),
        ]
    } else {
        Vec::new()
    };
    AttachmentList {
        source,
        attachments,
        warnings,
    }
}

pub async fn list_attachments(source: AttachmentSource) -> Result<AttachmentList> {
    validate_attachment_source(&source)?;
    match &source {
        AttachmentSource::Documentation { document_id } => Ok(attachment_list_result(
            source.clone(),
            get_documentation(document_id).await?.attachments,
        )),
        AttachmentSource::Feedback {
            document_id,
            feedback_ordinal,
        } => Ok(attachment_list_result(
            source.clone(),
            get_feedback(document_id, *feedback_ordinal)
                .await?
                .attachments,
        )),
        AttachmentSource::CompetencyGoal { goal_number } => Ok(attachment_list_result(
            source.clone(),
            competency_goal(*goal_number).await?.attachments,
        )),
        AttachmentSource::Delmal {
            goal_number,
            delmal_number,
        } => {
            let attachments = delmal_attachments(*goal_number, *delmal_number).await?;
            if attachments.is_empty() {
                bail!(
                    "the current Fagbrev UI does not expose an independent attachment control for this delmål"
                );
            }
            Ok(attachment_list_result(source.clone(), attachments))
        }
    }
}

fn attachment_click_script(
    source: &AttachmentSource,
    ordinal: u32,
    delmal_title: Option<&str>,
) -> Result<String> {
    if ordinal == 0 {
        bail!("attachment ordinal must be greater than zero");
    }
    let index = usize::try_from(ordinal - 1).context("attachment ordinal is too large")?;
    let extractor = "const attachment = (root, index) => { const items = Array.from(root?.querySelectorAll('.FFileViewerBtn') || []); const item = items[index]; if (!item) return false; (item.matches('a,button') ? item : item.querySelector('a,button') || item).click(); return true; };";
    match source {
        AttachmentSource::Documentation { .. } => Ok(format!(
            r#"() => {{
            {extractor}
            const panelButton = Array.from(document.querySelectorAll('main button')).find(button => (button.innerText || '').trim().startsWith('Vedlegg ('));
            return attachment(panelButton?.closest('.FExpantion'), {index});
        }}"#
        )),
        AttachmentSource::Feedback {
            feedback_ordinal, ..
        } => {
            let feedback_index = feedback_ordinal
                .checked_sub(1)
                .context("feedback ordinal must be greater than zero")?;
            Ok(format!(
                r#"() => {{
                {extractor}
                const card = Array.from(document.querySelectorAll('main .vcard')).find(element => Array.from(element.querySelectorAll('h3')).some(heading => heading.innerText.trim() === 'Tilbakemeldinger'));
                const entries = Array.from(card?.querySelectorAll('.divide-y > div') || []).filter(item => (item.querySelector('p')?.innerText || '').trim());
                return attachment(entries[{feedback_index}], {index});
            }}"#
            ))
        }
        AttachmentSource::CompetencyGoal { goal_number } => Ok(format!(
            r#"() => {{
            {extractor}
            const button = Array.from(document.querySelectorAll('main button')).find(element => new RegExp('(?:^|\\s){}\\.\\s').test((element.innerText || '').replace(/\s+/g, ' ').trim()));
            return attachment(button?.parentElement, {index});
        }}"#,
            goal_number
        )),
        AttachmentSource::Delmal { goal_number, .. } => {
            let title = delmal_title.context("delmål title is required for attachment download")?;
            let title = serde_json::to_string(title)?;
            Ok(format!(
                r#"() => {{
                {extractor}
                const button = Array.from(document.querySelectorAll('main button')).find(element => new RegExp('(?:^|\\s){}\\.\\s').test((element.innerText || '').replace(/\s+/g, ' ').trim()));
                const root = button?.parentElement;
                const target = {title};
                const items = Array.from(root?.querySelectorAll('.FFileViewerBtn') || []).filter(element => {{
                    let node = element.parentElement;
                    while (node && node !== root) {{
                        if ((node.innerText || '').replace(/\s+/g, ' ').trim().includes(target)) return true;
                        node = node.parentElement;
                    }}
                    return false;
                }});
                const item = items[{index}];
                if (!item) return false;
                (item.matches('a,button') ? item : item.querySelector('a,button') || item).click();
                return true;
            }}"#,
                goal_number
            ))
        }
    }
}

async fn wait_for_download(download_dir: &Path) -> Result<PathBuf> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let mut candidates = Vec::new();
        for entry in fs::read_dir(download_dir)? {
            let path = entry?.path();
            if path.is_file()
                && path
                    .extension()
                    .is_none_or(|extension| extension != "crdownload")
            {
                candidates.push(path);
            }
        }
        if candidates.len() == 1 {
            if fs::metadata(&candidates[0])?.len() > MAX_ATTACHMENT_BYTES {
                bail!("attachment exceeds the {MAX_ATTACHMENT_BYTES} byte download limit");
            }
            return Ok(candidates.remove(0));
        }
        if candidates.len() > 1 {
            bail!("attachment download produced more than one file");
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "the visible attachment control did not produce a completed download within 30 seconds"
            );
        }
        sleep(Duration::from_millis(250)).await;
    }
}

async fn download_from_page(
    browser: &Browser,
    page: &chromiumoxide::Page,
    source: &AttachmentSource,
    ordinal: u32,
    delmal_title: Option<&str>,
    output_path: &Path,
    overwrite: bool,
) -> Result<u64> {
    let parent = output_path
        .parent()
        .context("output_path has no parent directory")?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    let download_dir = parent.join(format!(
        ".fagbrev-mcp-download-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir(&download_dir).with_context(|| {
        format!(
            "could not create temporary download directory {}",
            download_dir.display()
        )
    })?;
    let result = async {
        browser
            .execute(
                SetDownloadBehaviorParams::builder()
                    .behavior(SetDownloadBehaviorBehavior::Allow)
                    .download_path(download_dir.to_string_lossy())
                    .build()
                    .map_err(|error| anyhow::anyhow!(error))?,
            )
            .await?;
        let clicked: bool = page
            .evaluate(attachment_click_script(source, ordinal, delmal_title)?)
            .await?
            .into_value()?;
        if !clicked {
            bail!("the selected attachment control is not exposed by the current Fagbrev UI");
        }
        wait_for_download(&download_dir).await
    }
    .await;
    let result = match result {
        Ok(temp) => finalize_download(&temp, output_path, overwrite),
        Err(error) => Err(error),
    };
    let _ = fs::remove_dir_all(&download_dir);
    result
}

pub async fn download_attachment(
    source: AttachmentSource,
    ordinal: u32,
    output_path: &str,
    overwrite: bool,
) -> Result<AttachmentDownloadResult> {
    validate_attachment_source(&source)?;
    let output_path = validate_output_path(output_path, overwrite)?;
    let available = list_attachments(source.clone()).await?;
    let attachment = available
        .attachments
        .iter()
        .find(|attachment| attachment.ordinal == ordinal)
        .with_context(|| format!("could not find attachment {ordinal} for the selected source"))?;
    let metadata = attachment.clone();
    let mut delmal_title_value = None;
    let (browser, handler_task, page, owns_browser) = match &source {
        AttachmentSource::Documentation { document_id }
        | AttachmentSource::Feedback { document_id, .. } => {
            let session = open_page(&format!("{DOCUMENTATION_URL}/{document_id}")).await?;
            wait_for_text(&session.2, "Sist endret:").await?;
            wait_for_feedback_section(&session.2).await?;
            let _ = click_documentation_expansion(&session.2, "Vedlegg (").await?;
            session
        }
        AttachmentSource::CompetencyGoal { goal_number } => {
            let session = plan_page(false).await?;
            if let Err(error) = expand_goals(&session.2, &[*goal_number]).await {
                finish(session.0, session.1, session.3).await?;
                return Err(error);
            }
            session
        }
        AttachmentSource::Delmal {
            goal_number,
            delmal_number,
        } => {
            let session = plan_page(false).await?;
            match delmal_title(&session.2, *goal_number, *delmal_number).await {
                Ok(title) => delmal_title_value = Some(title),
                Err(error) => {
                    finish(session.0, session.1, session.3).await?;
                    return Err(error);
                }
            }
            session
        }
    };
    let result = download_from_page(
        &browser,
        &page,
        &source,
        ordinal,
        delmal_title_value.as_deref(),
        &output_path,
        overwrite,
    )
    .await;
    let result = result.map(|bytes_written| AttachmentDownloadResult {
        source: source.clone(),
        attachment_ordinal: ordinal,
        name: metadata.name,
        mime_type: metadata.mime_type,
        output_path: output_path.display().to_string(),
        bytes_written,
        overwritten: overwrite,
    });
    finish(browser, handler_task, owns_browser).await?;
    result
}

async fn finish(
    browser: BrowserHandle,
    handler_task: Option<tokio::task::JoinHandle<()>>,
    close_browser: bool,
) -> Result<()> {
    match browser {
        BrowserHandle::Owned(mut browser) => {
            if close_browser {
                browser.close().await?;
                browser.wait().await?;
            }
        }
        BrowserHandle::Shared { page, .. } => {
            if let Err(error) = page.close().await {
                tracing::debug!(%error, "could not close a shared-browser page");
            }
        }
    }
    if let Some(handler_task) = handler_task {
        handler_task.abort();
    }
    Ok(())
}

pub async fn login() -> Result<()> {
    let profile = profile_dir()?;
    println!("Opening Fagbrev.io in a dedicated browser profile.");
    println!("Profile: {}", profile.display());
    println!(
        "Complete the normal login in the browser window; no credentials are read by this CLI."
    );

    let (browser, handler_task, page, owns_browser) = open_dashboard(true).await?;
    let result = timeout(LOGIN_WAIT, async {
        loop {
            let snapshot = page_snapshot(&page).await?;
            if is_authenticated(&snapshot) {
                return Ok::<PageSnapshot, anyhow::Error>(snapshot);
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await;

    let output = match result {
        Ok(Ok(snapshot)) => {
            println!("Login detected on the authenticated dashboard.");
            print_json(&dashboard_overview_from_snapshot(snapshot))
        }
        Ok(Err(error)) => Err(error),
        Err(_) => bail!("timed out waiting for login; the browser profile was preserved"),
    };

    finish(browser, handler_task, owns_browser).await?;
    output
}

pub async fn status() -> Result<()> {
    print_json(&dashboard_overview().await?)
}

pub async fn inspect() -> Result<()> {
    let (browser, handler_task, page, owns_browser) = open_dashboard(false).await?;
    let snapshot = wait_for_text(&page, "Fagbrev").await?;
    let result = print_json(&snapshot);
    finish(browser, handler_task, owns_browser).await?;
    result
}

pub fn logout() -> Result<()> {
    let profile = profile_dir()?;
    if !profile.exists() {
        println!("No local browser profile exists.");
        return Ok(());
    }

    print!(
        "Remove the local Fagbrev browser profile at {}? [y/N] ",
        profile.display()
    );
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if answer.trim().eq_ignore_ascii_case("y") {
        std::fs::remove_dir_all(&profile)?;
        println!("Local browser profile removed.");
    } else {
        println!("Kept the local browser profile.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        DocumentationDetailSnapshot, DocumentationRowSnapshot, DocumentationTableReadiness,
        FeedbackSnapshot, HalfYearTaskSnapshot, PageSnapshot, dashboard_overview_from_snapshot,
        delete_unsupported_result, documentation_list_url, documentation_page_size_for_limit,
        documentation_record_from_detail, documentation_status_from_ui,
        documentation_table_is_rendered, goal_list_error, goal_list_is_complete,
        parse_documentation_rows, parse_expanded_goal_from_page_text, parse_feedback,
        parse_goals_from_page_text, parse_half_year_tasks, validate_documentation_target,
    };
    use crate::data::{CompetencyGoalReference, Delmal, DocumentationStatus, DocumentationTarget};

    #[test]
    fn parses_dashboard_progress_and_counts() {
        let overview = dashboard_overview_from_snapshot(PageSnapshot {
            title: "Fagbrev - Oversikt".to_string(),
            url: "https://fagbrev.io/l".to_string(),
            text: "Velkommen. Du har fullført 38% av læretiden din, skrevet 15 dokumentasjoner og hatt 4 vurderingssamtaler. 1 Dokumentasjoner som må rettes 0 Dokumentasjoner til vurdering 14 Godkjente dokumentasjoner".to_string(),
        });

        assert!(overview.authenticated);
        assert_eq!(overview.progress_percent, Some(38));
        assert_eq!(overview.documentations_written, Some(15));
        assert_eq!(overview.assessment_conversations, Some(4));
        assert_eq!(overview.documentations_needing_correction, Some(1));
        assert_eq!(overview.documentations_to_review, Some(0));
        assert_eq!(overview.approved_documentations, Some(14));
    }

    #[test]
    fn parses_goals_from_accessible_page_text() {
        let goals = parse_goals_from_page_text(
            "1\n1. Planlegge, utvikle og dokumentere løsninger\n0\n4. Planlegge systemer for datainnsamling",
        );

        assert_eq!(goals.len(), 2);
        assert_eq!(goals[0].number, 1);
        assert_eq!(goals[1].status_count, 0);
    }

    #[test]
    fn parses_goals_when_the_ui_renders_counts_after_headings() {
        let goals = parse_goals_from_page_text(
            "1. Planlegge, utvikle og dokumentere løsninger\n3\n2. Planlegge og vurdere brukervennlighet\n0",
        );

        assert_eq!(goals.len(), 2);
        assert_eq!(goals[0].number, 1);
        assert_eq!(goals[0].status_count, 3);
        assert_eq!(goals[1].number, 2);
        assert_eq!(goals[1].status_count, 0);
    }

    #[test]
    fn detects_empty_or_partial_goal_results_instead_of_accepting_them() {
        let snapshot = PageSnapshot {
            title: "Fagbrev - Læreplanmål".to_string(),
            url: "https://fagbrev.io/l/laereplanmal".to_string(),
            text: "KOMPETANSEMÅL".to_string(),
        };
        let error = goal_list_error(&[], &snapshot);

        assert!(!goal_list_is_complete(&[]));
        assert!(
            error
                .to_string()
                .contains("could not parse the 21 competency goals")
        );
    }

    #[test]
    fn chooses_the_smallest_supported_documentation_page_size_for_a_limit() {
        assert_eq!(documentation_page_size_for_limit(1), 6);
        assert_eq!(documentation_page_size_for_limit(6), 6);
        assert_eq!(documentation_page_size_for_limit(7), 12);
        assert_eq!(documentation_page_size_for_limit(20), 24);
        assert_eq!(documentation_page_size_for_limit(200), 200);
        assert_eq!(documentation_page_size_for_limit(201), 200);
    }

    #[test]
    fn parses_expanded_goal_bullets_and_joins_wrapped_text() {
        let (title, status_count, delmal) = parse_expanded_goal_from_page_text(
            "1\n1. Parent goal\nPlanlegging:\n * First activity continued\n   text\n * Second activity\nUtvikling:\n * Third activity\nGjennom hele prosessen kan lærlingen bruke interne rutiner.\nSe tilknyttede dokumentasjoner (1)\n2\n2. Next goal",
            1,
        );

        assert_eq!(title.as_deref(), Some("Parent goal"));
        assert_eq!(status_count, Some(1));
        assert_eq!(delmal.len(), 3);
        assert_eq!(delmal[0].number, Some(1));
        assert_eq!(delmal[0].title, "First activity continued text");
        assert_eq!(delmal[2].goal_number, 1);
    }

    #[test]
    fn prefers_the_rendered_delmal_list_over_suggested_task_bullets() {
        let (title, status_count, delmal) = parse_expanded_goal_from_page_text(
            "0\n12. Beskrive krav til infrastruktur\n\nForslag til oppgaver:\n * Lage og dokumentere nettverkskisse\n * Foreslå og begrunne mulige tilpasninger\n\nDelmål ( 3 )\n\n0\n\nForklare forskjeller på skytjenester og on-premise løsninger\n\n2\n\nLage og dokumentere nettverkskisse\n\n0\n\nDokumentere infrastrukturstandarder\n\nSe tilknyttede dokumentasjoner (0)\nNy dokumentasjon\n0\n13. Next goal",
            12,
        );

        assert_eq!(title.as_deref(), Some("Beskrive krav til infrastruktur"));
        assert_eq!(status_count, Some(0));
        let titles = delmal
            .iter()
            .map(|item| item.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            titles,
            [
                "Forklare forskjeller på skytjenester og on-premise løsninger",
                "Lage og dokumentere nettverkskisse",
                "Dokumentere infrastrukturstandarder",
            ]
        );
        assert_eq!(delmal[1].number, Some(2));
        assert_eq!(delmal[1].status_count, Some(2));
    }

    #[test]
    fn documentation_list_url_always_resets_search_state() {
        assert_eq!(
            documentation_list_url(1, 100, None),
            "https://fagbrev.io/l/dokumentasjon?size=100&p=1&q="
        );
        assert_eq!(
            documentation_list_url(2, 6, Some(" mål 12&x ")),
            "https://fagbrev.io/l/dokumentasjon?size=6&p=2&q=m%C3%A5l%2012%26x"
        );
    }

    #[test]
    fn raw_string_scripts_do_not_double_escape_regex_literals() {
        // Inside r#"..."# a `\\s` reaches JavaScript as a literal backslash,
        // which silently broke target matching in the documentation picker.
        let source = include_str!("browser.rs");
        for script in source.split("r#\"").skip(1) {
            let script = script.split("\"#").next().unwrap_or_default();
            for pattern in ["/\\\\s", "/^(\\\\d", "(\\\\d+)\\\\."] {
                assert!(
                    !script.contains(pattern),
                    "double-escaped regex {pattern} in raw script"
                );
            }
        }
    }

    #[test]
    fn parses_documentation_rows_with_real_ids_and_ui_statuses() {
        let rows = parse_documentation_rows(vec![
            DocumentationRowSnapshot {
                id: Some("realApprovedId".to_string()),
                url: Some("/l/dokumentasjon/realApprovedId".to_string()),
                title: Some("Approved work".to_string()),
                updated_at: Some("03.06.2026 10:00".to_string()),
                status_icon: Some("check-circle".to_string()),
                status_color: Some("bg-positive".to_string()),
            },
            DocumentationRowSnapshot {
                id: Some("realCorrectionId".to_string()),
                url: Some("/l/dokumentasjon/realCorrectionId".to_string()),
                title: Some("Needs correction".to_string()),
                updated_at: Some("23.06.2026 13:17".to_string()),
                status_icon: Some("edit-2".to_string()),
                status_color: Some("bg-negative".to_string()),
            },
            DocumentationRowSnapshot {
                id: Some("realDraftId".to_string()),
                url: Some("/l/dokumentasjon/realDraftId".to_string()),
                title: Some("Draft".to_string()),
                updated_at: Some("11.09.2026 10:24".to_string()),
                status_icon: Some("edit".to_string()),
                status_color: Some("bg-dark".to_string()),
            },
        ]);

        assert_eq!(rows[0].id.as_deref(), Some("realApprovedId"));
        assert_eq!(rows[0].status, DocumentationStatus::Approved);
        assert_eq!(rows[1].status, DocumentationStatus::NeedsCorrection);
        assert_eq!(rows[2].status, DocumentationStatus::Draft);
        assert_eq!(
            documentation_status_from_ui(Some("unknown"), None, None),
            DocumentationStatus::Unknown
        );
    }

    #[test]
    fn accepts_a_rendered_empty_documentation_page() {
        let readiness = DocumentationTableReadiness {
            has_table: true,
            linked_rows: 0,
            body_text: "Søk\nViser\n0\nav 0".to_string(),
        };

        assert!(documentation_table_is_rendered(&readiness));
        assert!(!documentation_table_is_rendered(
            &DocumentationTableReadiness {
                has_table: true,
                linked_rows: 0,
                body_text: "Søk\nLaster inn".to_string(),
            }
        ));
    }

    #[test]
    fn parses_documentation_detail_content_attachments_and_targets() {
        let record = documentation_record_from_detail(
            "realDocumentId".to_string(),
            "https://fagbrev.io/l/dokumentasjon/realDocumentId".to_string(),
            DocumentationDetailSnapshot {
                title: Some("My documentation".to_string()),
                status_label: Some("Godkjent".to_string()),
                updated_at: Some("03.06.2026".to_string()),
                content_text: Some("Visible documentation text".to_string()),
                content_html: Some("<p>Visible documentation text</p>".to_string()),
                attachments: vec![crate::data::AttachmentMetadata {
                    ordinal: 1,
                    name: Some("evidence.docx".to_string()),
                    url: None,
                    mime_type: None,
                    size_bytes: None,
                }],
                target_text: Some(
                    "LÆREPLAN\nLæreplan i IT-utviklerfaget\nKompetansemål og vurdering vg3 IT-utviklerfaget\n1 mål og 1 delmål\n15. Feilsøke kode\nDokumentere virksomhetens rutiner for feilsøking\n1 dok.".to_string(),
                ),
                feedback: vec![],
            },
        );

        assert_eq!(record.id.as_deref(), Some("realDocumentId"));
        assert_eq!(record.status, DocumentationStatus::Approved);
        assert_eq!(
            record.content.as_deref(),
            Some("Visible documentation text")
        );
        assert_eq!(
            record.content_html.as_deref(),
            Some("<p>Visible documentation text</p>")
        );
        assert_eq!(record.attachments[0].name.as_deref(), Some("evidence.docx"));
        assert_eq!(record.attachments[0].ordinal, 1);
        let target = record.target_summary.expect("target summary");
        assert_eq!(target.competency_goals[0].number, 15);
        assert_eq!(target.delmal[0].id, None);
        assert_eq!(
            target.delmal[0].title,
            "Dokumentere virksomhetens rutiner for feilsøking"
        );
    }

    #[test]
    fn parses_half_year_task_fixture_with_ui_ordinals_and_counts() {
        let tasks = parse_half_year_tasks(vec![
            HalfYearTaskSnapshot {
                ordinal: 1,
                title: "Etikk, lovverk og yrkesutøvelse".to_string(),
                task_text: Some("Planlegg, gjennomfør, dokumenter og evaluer.".to_string()),
                status_label: None,
                status_icon: None,
                status_color: None,
                documentation_count: Some(2),
                linked_documentation: vec![crate::data::LinkedDocumentationSummary {
                    id: Some("doc-1".to_string()),
                    title: Some("Etikk i praksis".to_string()),
                    status: Some(DocumentationStatus::Approved),
                }],
            },
            HalfYearTaskSnapshot {
                ordinal: 6,
                title: "Minifagprøve".to_string(),
                task_text: None,
                status_label: Some("Godkjent".to_string()),
                status_icon: Some("check-circle".to_string()),
                status_color: Some("bg-positive".to_string()),
                documentation_count: Some(1),
                linked_documentation: vec![],
            },
        ]);

        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].ordinal, 1);
        assert_eq!(tasks[0].documentation_count, Some(2));
        assert_eq!(
            tasks[0].linked_documentation[0].id.as_deref(),
            Some("doc-1")
        );
        assert_eq!(tasks[0].status, None);
        assert_eq!(tasks[1].status, Some(DocumentationStatus::Approved));
        assert_eq!(tasks[1].task_text, None);
    }

    #[test]
    fn parses_feedback_fixture_without_avatar_or_other_private_data() {
        let feedback = parse_feedback(
            "realDocumentId",
            vec![FeedbackSnapshot {
                status_label: Some("Godkjent".to_string()),
                status_icon: Some("check-circle".to_string()),
                status_color: Some("bg-positive".to_string()),
                author: Some("Rune Alexander Laursen".to_string()),
                timestamp: Some("20.06.2026 11:08".to_string()),
                text: "Bra dokumentasjon.".to_string(),
                attachments: vec![],
            }],
        );

        assert_eq!(feedback.len(), 1);
        assert_eq!(feedback[0].ordinal, 1);
        assert_eq!(feedback[0].document_id, "realDocumentId");
        assert_eq!(feedback[0].status, Some(DocumentationStatus::Approved));
        assert_eq!(
            feedback[0].author.as_deref(),
            Some("Rune Alexander Laursen")
        );
        assert_eq!(feedback[0].timestamp.as_deref(), Some("20.06.2026 11:08"));
        assert_eq!(feedback[0].text, "Bra dokumentasjon.");
        assert!(feedback[0].attachments.is_empty());
    }

    #[test]
    fn validates_write_targets_without_browser_access() {
        let valid = DocumentationTarget {
            competency_goal: Some(CompetencyGoalReference {
                number: 15,
                title: None,
                id: None,
            }),
            delmal: Some(Delmal {
                goal_number: 15,
                number: Some(1),
                title: "Dokumentere rutiner".to_string(),
                id: None,
                status: None,
                status_count: None,
            }),
        };
        assert!(validate_documentation_target(&valid).is_ok());

        let mismatched = DocumentationTarget {
            delmal: Some(Delmal {
                goal_number: 14,
                number: Some(1),
                title: "Dokumentere rutiner".to_string(),
                ..valid.delmal.clone().expect("delmål")
            }),
            ..valid.clone()
        };
        assert!(validate_documentation_target(&mismatched).is_err());

        let missing_ordinal = DocumentationTarget {
            delmal: Some(Delmal {
                number: None,
                ..valid.delmal.expect("delmål")
            }),
            competency_goal: None,
        };
        assert!(validate_documentation_target(&missing_ordinal).is_err());
    }

    #[test]
    fn verifies_saved_record_and_rejects_untrusted_results() {
        use crate::data::{DocumentationRecord, DocumentationTargetSummary};
        let goal = CompetencyGoalReference {
            number: 12,
            title: None,
            id: None,
        };
        let target = DocumentationTarget {
            competency_goal: Some(goal.clone()),
            delmal: None,
        };
        let record = DocumentationRecord {
            id: Some("doc123".into()),
            url: Some("https://fagbrev.io/l/dokumentasjon/doc123".into()),
            title: Some("Tittel".into()),
            content: Some("Linje 1\nLinje 2".into()),
            target_summary: Some(DocumentationTargetSummary {
                competency_goals: vec![goal],
                ..Default::default()
            }),
            ..Default::default()
        };
        let verify = |record: &DocumentationRecord| {
            super::verify_saved_documentation(
                record,
                "doc123",
                "Tittel",
                "Linje 1\nLinje 2",
                &target,
            )
        };
        assert!(verify(&record).is_ok());
        let mut changed = record.clone();
        changed.content = None;
        assert!(verify(&changed).is_err());
        changed.content = Some("annet".into());
        assert!(verify(&changed).is_err());
        changed = record.clone();
        changed.title = Some("Ingen tittel".into());
        assert!(verify(&changed).is_err());
        changed = record.clone();
        changed.id = Some("wrong".into());
        assert!(verify(&changed).is_err());
        changed = record.clone();
        changed.url = Some("https://fagbrev.io/l/dokumentasjon/wrong".into());
        assert!(verify(&changed).is_err());
        changed = record.clone();
        changed.target_summary = None;
        assert!(verify(&changed).is_err());
        changed = record.clone();
        changed
            .target_summary
            .as_mut()
            .unwrap()
            .competency_goals
            .push(CompetencyGoalReference {
                number: 5,
                title: None,
                id: None,
            });
        assert!(verify(&changed).is_err());
        assert_eq!(
            documentation_status_from_ui(Some("Sendt"), None, None),
            DocumentationStatus::Submitted
        );
    }

    #[tokio::test]
    async fn browser_write_methods_refuse_without_confirmation() {
        let target = DocumentationTarget::default();
        let submit_error = super::submit_documentation("title", "content", &target, false)
            .await
            .expect_err("unconfirmed submit must stop before browser access");
        assert!(submit_error.to_string().contains("confirm=true"));

        let approval_error =
            super::request_approval("document-123", "title", "content", &target, false)
                .await
                .expect_err("unconfirmed approval must stop before browser access");
        assert!(approval_error.to_string().contains("confirm=true"));

        let update_error = super::update_documentation(
            "document-123",
            "title",
            "content",
            None,
            "11.09.2026",
            false,
        )
        .await
        .expect_err("unconfirmed update must stop before browser access");
        assert!(update_error.to_string().contains("confirm=true"));

        let delete_error = super::delete_documentation(
            "document-123",
            DocumentationStatus::Draft,
            "11.09.2026",
            false,
        )
        .await
        .expect_err("unconfirmed delete must stop before browser access");
        assert!(delete_error.to_string().contains("confirm=true"));
    }

    #[test]
    fn unsupported_delete_result_is_structured_and_non_mutating() {
        let result = delete_unsupported_result(
            "draft-123",
            DocumentationStatus::Draft,
            "11.09.2026",
            DocumentationStatus::Draft,
        );

        assert_eq!(result.outcome, "unsupported");
        assert!(!result.would_mutate);
        assert!(!result.mutated);
        assert!(result.message.contains("clearly labelled delete action"));
    }
}
