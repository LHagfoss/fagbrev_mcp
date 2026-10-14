use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use rmcp::{
    ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::CallToolResult,
    schemars::JsonSchema,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};

use crate::browser;
use crate::data::{
    AttachmentSource, CompetencyGoalReference, DeleteDocumentationResult, Delmal,
    DocumentationStatus, DocumentationTarget, DocumentationUpdatePreview,
    DocumentationWritePreview, DraftDeleteResult, DraftWriteResult, RequestApprovalResult,
    SubmitDocumentationResult, UpdateDocumentationResult,
};
use crate::{context, drafts};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FagbrevServer {
    tool_router: ToolRouter<Self>,
    _marker: Arc<()>,
}

impl Default for FagbrevServer {
    fn default() -> Self {
        let mut tool_router = Self::tool_router();
        for route in tool_router.map.values_mut() {
            let mut schema = serde_json::Value::Object((*route.attr.input_schema).clone());
            compact_schema(&mut schema);
            if let serde_json::Value::Object(object) = schema {
                route.attr.input_schema = Arc::new(object);
            }
        }
        Self {
            tool_router,
            _marker: Arc::new(()),
        }
    }
}

/// Strip schemars noise that costs tokens without guiding callers: the
/// `$schema` URI, integer `format`s, implied unsigned bounds, `default: null`,
/// `default` values of null/false/[], and `null` in optional field types (optional fields are simply omitted).
fn compact_schema(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Object(object) => {
            object.remove("$schema");
            object.remove("format");
            if object.get("default").is_some_and(|default| {
                default.is_null()
                    || default == &Value::Bool(false)
                    || default.as_array().is_some_and(Vec::is_empty)
            }) {
                object.remove("default");
            }
            if object.get("minimum").and_then(Value::as_u64) == Some(0) {
                object.remove("minimum");
            }
            if object
                .get("maximum")
                .and_then(Value::as_u64)
                .is_some_and(|max| [255, 65_535, u64::from(u32::MAX)].contains(&max))
            {
                object.remove("maximum");
            }
            if let Some(Value::Array(types)) = object.get_mut("type") {
                types.retain(|kind| kind != "null");
                if types.len() == 1 {
                    let single = types.remove(0);
                    object.insert("type".to_string(), single);
                }
            }
            if let Some(Value::Array(variants)) = object.get_mut("enum") {
                variants.retain(|variant| !variant.is_null());
            }
            for child in object.values_mut() {
                compact_schema(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(compact_schema),
        _ => {}
    }
}

/// Flat competency-goal/delmål target used by write and composition tools.
///
/// Converted into the internal [`DocumentationTarget`]. Without a delmål the
/// whole goal is targeted; with a delmål the goal is also selected only when
/// `goal_title` is given.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TargetArgs {
    /// Competency goal number (1-21).
    pub goal_number: u8,
    /// Goal title (needed for reuse matching); with a delmål it also selects the goal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_title: Option<String>,
    /// Delmål ordinal in the goal; omit to target the whole goal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delmal_number: Option<u16>,
    /// Exact delmål title (required with delmal_number).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delmal_title: Option<String>,
}

impl TargetArgs {
    fn to_target(&self) -> Result<DocumentationTarget> {
        build_target(
            Some(self.goal_number),
            self.goal_title.clone(),
            self.delmal_number,
            self.delmal_title.clone(),
        )?
        .ok_or_else(|| anyhow!("goal_number is required"))
    }
}

/// Build a [`DocumentationTarget`] from flat fields. Returns `None` when every
/// field is omitted.
fn build_target(
    goal_number: Option<u8>,
    goal_title: Option<String>,
    delmal_number: Option<u16>,
    delmal_title: Option<String>,
) -> Result<Option<DocumentationTarget>> {
    let Some(goal_number) = goal_number else {
        if goal_title.is_some() || delmal_number.is_some() || delmal_title.is_some() {
            bail!("goal_number is required when any target field is given");
        }
        return Ok(None);
    };
    let goal = CompetencyGoalReference {
        number: goal_number,
        title: goal_title,
        id: None,
    };
    let target = match delmal_number {
        None => {
            if delmal_title.is_some() {
                bail!("delmal_title requires delmal_number");
            }
            DocumentationTarget {
                competency_goal: Some(goal),
                delmal: None,
            }
        }
        Some(number) => {
            let Some(title) = delmal_title else {
                bail!("delmal_title is required with delmal_number");
            };
            DocumentationTarget {
                competency_goal: goal.title.is_some().then_some(goal),
                delmal: Some(Delmal {
                    goal_number,
                    number: Some(number),
                    title,
                    id: None,
                    status: None,
                    status_count: None,
                }),
            }
        }
    };
    browser::validate_documentation_target(&target)?;
    Ok(Some(target))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DashboardRequest {
    /// Only the compact status summary.
    #[serde(default)]
    pub status_only: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CompetencyGoalsRequest {
    /// Goal 1-21; omit to list all goals.
    #[serde(default)]
    pub goal_number: Option<u8>,
    /// Delmål ordinal within goal_number; returns only that delmål.
    #[serde(default)]
    pub delmal_number: Option<u16>,
    /// Without goal_number: list every delmål of all goals (slow).
    #[serde(default)]
    pub all_delmal: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct HalfYearTasksRequest {
    /// Area 1-6; omit to list all.
    #[serde(default)]
    pub ordinal: Option<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DocumentationRequest {
    /// Record ID (/l/dokumentasjon/{id}); omit to list records.
    #[serde(default)]
    pub document_id: Option<String>,
    /// List: page, default 1.
    #[serde(default)]
    pub page: Option<u32>,
    /// List: 6 (default), 12, 18, 24, 100 or 200.
    #[serde(default)]
    pub page_size: Option<u32>,
    /// List: text search.
    #[serde(default)]
    pub query: Option<String>,
    /// List: status filter.
    #[serde(default)]
    pub status: Option<DocumentationStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FeedbackRequest {
    /// Documentation ID whose feedback to read.
    pub document_id: String,
    /// Newest-first ordinal; omit to list all.
    #[serde(default)]
    pub feedback_ordinal: Option<u32>,
}

/// Where attachments are read from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum AttachmentKind {
    Documentation,
    Feedback,
    CompetencyGoal,
    Delmal,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentSourceArgs {
    /// Owner: documentation/feedback use document_id (+feedback_ordinal); competency_goal/delmal use goal_number (+delmal_number).
    pub source: AttachmentKind,
    /// Record ID.
    #[serde(default)]
    pub document_id: Option<String>,
    /// Feedback ordinal.
    #[serde(default)]
    pub feedback_ordinal: Option<u32>,
    /// Goal 1-21.
    #[serde(default)]
    pub goal_number: Option<u8>,
    /// Delmål ordinal.
    #[serde(default)]
    pub delmal_number: Option<u16>,
}

impl AttachmentSourceArgs {
    fn to_source(&self) -> Result<AttachmentSource> {
        let document_id = || {
            self.document_id
                .clone()
                .ok_or_else(|| anyhow!("document_id is required for this source"))
        };
        let goal_number = || {
            self.goal_number
                .ok_or_else(|| anyhow!("goal_number is required for this source"))
        };
        Ok(match self.source {
            AttachmentKind::Documentation => AttachmentSource::Documentation {
                document_id: document_id()?,
            },
            AttachmentKind::Feedback => AttachmentSource::Feedback {
                document_id: document_id()?,
                feedback_ordinal: self
                    .feedback_ordinal
                    .ok_or_else(|| anyhow!("feedback_ordinal is required for feedback"))?,
            },
            AttachmentKind::CompetencyGoal => AttachmentSource::CompetencyGoal {
                goal_number: goal_number()?,
            },
            AttachmentKind::Delmal => AttachmentSource::Delmal {
                goal_number: goal_number()?,
                delmal_number: self
                    .delmal_number
                    .ok_or_else(|| anyhow!("delmal_number is required for delmal"))?,
            },
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentListRequest {
    #[serde(flatten)]
    pub source: AttachmentSourceArgs,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentDownloadRequest {
    #[serde(flatten)]
    pub source: AttachmentSourceArgs,
    /// Attachment ordinal from list_attachments.
    pub attachment_ordinal: u32,
    /// Absolute local file path to write.
    pub output_path: String,
    /// Allow replacing a file.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UpdateDocumentationRequest {
    /// Documentation ID to edit.
    pub document_id: String,
    /// New title.
    pub title: String,
    /// New plain-text content.
    pub content: String,
    /// Exact updated_at from get_documentation.
    pub expected_updated_at: String,
    /// New target goal; omit all target fields to keep the selection.
    #[serde(default)]
    pub goal_number: Option<u8>,
    /// New goal title (selects the goal too when a delmål is given).
    #[serde(default)]
    pub goal_title: Option<String>,
    /// New delmål ordinal.
    #[serde(default)]
    pub delmal_number: Option<u16>,
    /// New delmål exact title.
    #[serde(default)]
    pub delmal_title: Option<String>,
    /// true = actually save; default previews.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeleteDocumentationRequest {
    /// Documentation ID to delete.
    pub document_id: String,
    /// Must be draft.
    pub expected_status: DocumentationStatus,
    /// Exact updated_at from get_documentation.
    pub expected_updated_at: String,
    /// true = actually delete; default previews.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReusableContentRequest {
    #[serde(flatten)]
    pub target: TargetArgs,
    /// Extra required terms.
    #[serde(default)]
    pub query: Option<String>,
    /// Restrict to these record IDs.
    #[serde(default)]
    pub source_documentation_ids: Option<Vec<String>>,
    /// Restrict to this status.
    #[serde(default)]
    pub source_status: Option<DocumentationStatus>,
    /// Max suggestions.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DocumentationTemplateRequest {
    #[serde(flatten)]
    pub target: TargetArgs,
    /// Draft title.
    #[serde(default)]
    pub title: Option<String>,
    /// Record IDs to include as reference.
    #[serde(default)]
    pub source_documentation_ids: Vec<String>,
    /// Notes placed above the scaffold.
    #[serde(default)]
    pub extra_instructions: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SubmitDocumentationRequest {
    /// Title of the new entry.
    pub title: String,
    /// Plain-text content.
    pub content: String,
    #[serde(flatten)]
    pub target: TargetArgs,
    /// true = actually create; default previews.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RequestApprovalRequest {
    /// Documentation ID to send for approval.
    pub document_id: String,
    /// true = actually send; default previews.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SaveDraftRequest {
    /// Draft to update; omit to create.
    #[serde(default)]
    pub draft_id: Option<String>,
    /// Current version (required with draft_id).
    #[serde(default)]
    pub expected_version: Option<u64>,
    /// Draft title.
    pub title: String,
    /// Draft plain-text content.
    pub content: String,
    #[serde(flatten)]
    pub target: TargetArgs,
    /// Source record IDs.
    #[serde(default)]
    pub source_documentation_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DraftsRequest {
    /// Draft ID; omit to list summaries.
    #[serde(default)]
    pub draft_id: Option<String>,
    /// List: max items (default 50, max 100).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DraftRequest {
    /// Local draft ID.
    pub draft_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ContextBundleRequest {
    /// Include goal 1-21 with its delmål.
    #[serde(default)]
    pub goal_number: Option<u8>,
    /// Records to include.
    #[serde(default)]
    pub document_ids: Option<Vec<String>>,
    /// Max records (default 5, max 20).
    #[serde(default)]
    pub max_documents: Option<u32>,
    /// Include content of document_ids.
    #[serde(default)]
    pub include_document_content: bool,
}

const INSTRUCTIONS: &str = "Fagbrev: reads and manages the user's own Fagbrev.io apprenticeship dashboard (competency goals 1-21 with delmål, half-year tasks, documentation, feedback, attachments) through a logged-in local browser profile, plus local-only drafts.\n\
Write model: submit_documentation, update_documentation, delete_documentation and request_approval are preview-only unless confirm=true. Always call once without confirm, show the preview to the user, and only repeat with confirm=true after explicit user approval. Updates/deletes need the exact updated_at from get_documentation.\n\
Typical workflow: get_dashboard_overview -> get_competency_goals (pick goal/delmål numbers+titles) -> get_documentation / find_reusable_content -> make_documentation_template -> save_draft / get_drafts -> submit_documentation (preview, then confirm) -> request_approval.\n\
Targets are flat fields: goal_number, goal_title, delmal_number, delmal_title. Ordinals are UI-local, never backend IDs.";

const NEW_FORM_WARNING: &str = "Opening the new documentation form may create a blank draft. No form is opened and no browser data is changed until confirm=true.";

fn json_success<T: Serialize>(value: &T) -> CallToolResult {
    match serde_json::to_string_pretty(value) {
        Ok(json) => CallToolResult::success(vec![rmcp::model::ContentBlock::text(json)]),
        Err(error) => json_error(format!("Could not serialize tool result: {error}")),
    }
}

fn json_error(error: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![rmcp::model::ContentBlock::text(error.to_string())])
}

fn json_result<T: Serialize>(result: Result<T>, context: &str) -> CallToolResult {
    match result {
        Ok(value) => json_success(&value),
        Err(error) => json_error(format!("{context}: {error:#}")),
    }
}

fn submit_preview(
    request: &SubmitDocumentationRequest,
    target: &DocumentationTarget,
) -> Result<DocumentationWritePreview> {
    browser::validate_documentation_target(target)?;
    if request.title.trim().is_empty() {
        bail!("title cannot be empty");
    }
    if request.content.trim().is_empty() {
        bail!("content cannot be empty");
    }
    Ok(DocumentationWritePreview {
        operation: "submit_documentation".to_string(),
        confirmation_required: true,
        would_mutate: true,
        title: request.title.clone(),
        content: request.content.clone(),
        target: target.clone(),
        warning: NEW_FORM_WARNING.to_string(),
    })
}

fn approval_preview(document_id: String) -> RequestApprovalResult {
    RequestApprovalResult {
        outcome: "confirmation_required".to_string(),
        confirmation_required: true,
        would_mutate: true,
        mutated: false,
        document_id,
        url: None,
        status: None,
        message: "Preview only. If the record exposes the visible Send inn action, confirm=true will click it to request approval. No browser page was opened.".to_string(),
    }
}

const UPDATE_WARNING: &str = "No edit page is opened and no browser data changes until confirm=true. The expected_updated_at guard must still match the visible record before saving.";
const DELETE_WARNING: &str = "No documentation page is opened and nothing changes until confirm=true. Deletion is limited to drafts with an explicit status and updated_at guard; the current UI may report unsupported.";

fn valid_documentation_id(document_id: &str) -> Result<()> {
    if document_id.is_empty()
        || !document_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("document_id must be the ID from a Fagbrev documentation link");
    }
    Ok(())
}

fn update_preview(request: &UpdateDocumentationRequest) -> Result<DocumentationUpdatePreview> {
    valid_documentation_id(&request.document_id)?;
    if request.title.trim().is_empty() {
        bail!("title cannot be empty");
    }
    if request.content.trim().is_empty() {
        bail!("content cannot be empty");
    }
    if request.expected_updated_at.trim().is_empty() {
        bail!("expected_updated_at is required for safe documentation updates");
    }
    let replacement_target = build_target(
        request.goal_number,
        request.goal_title.clone(),
        request.delmal_number,
        request.delmal_title.clone(),
    )?;
    Ok(DocumentationUpdatePreview {
        operation: "update_documentation".to_string(),
        confirmation_required: true,
        would_mutate: true,
        document_id: request.document_id.clone(),
        title: request.title.clone(),
        content: request.content.clone(),
        replacement_target,
        expected_updated_at: request.expected_updated_at.clone(),
        warning: UPDATE_WARNING.to_string(),
    })
}

fn delete_preview(request: &DeleteDocumentationRequest) -> Result<DeleteDocumentationResult> {
    valid_documentation_id(&request.document_id)?;
    if request.expected_status != DocumentationStatus::Draft {
        bail!("delete_documentation only permits expected_status=draft");
    }
    if request.expected_updated_at.trim().is_empty() {
        bail!("expected_updated_at is required for safe documentation deletion");
    }
    Ok(DeleteDocumentationResult {
        outcome: "confirmation_required".to_string(),
        confirmation_required: true,
        would_mutate: true,
        mutated: false,
        document_id: request.document_id.clone(),
        expected_status: request.expected_status,
        expected_updated_at: request.expected_updated_at.clone(),
        status: None,
        message: DELETE_WARNING.to_string(),
    })
}

#[tool_router]
impl FagbrevServer {
    #[tool(
        name = "get_dashboard_overview",
        description = "Read the Fagbrev.io dashboard overview (or just progress/status).",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn get_dashboard_overview(
        &self,
        Parameters(request): Parameters<DashboardRequest>,
    ) -> CallToolResult {
        match browser::dashboard_overview().await {
            Ok(overview) if request.status_only => json_success(&overview.status()),
            Ok(overview) => json_success(&overview),
            Err(error) => json_error(format!("Could not read dashboard: {error:#}")),
        }
    }

    #[tool(
        name = "get_competency_goals",
        description = "Read the learning plan: all 21 competency goals, one goal with its delmål, or one delmål.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn get_competency_goals(
        &self,
        Parameters(request): Parameters<CompetencyGoalsRequest>,
    ) -> CallToolResult {
        if let Some(goal_number) = request.goal_number
            && !(1..=21).contains(&goal_number)
        {
            return json_error("goal_number must be between 1 and 21");
        }
        match (request.goal_number, request.delmal_number) {
            (None, Some(_)) => json_error("delmal_number requires goal_number"),
            (Some(goal), Some(delmal)) => json_result(
                browser::get_delmal(goal, delmal).await,
                "Could not read delmål",
            ),
            (Some(goal), None) => json_result(
                browser::competency_goal(goal).await,
                "Could not read competency goal",
            ),
            (None, None) if request.all_delmal => {
                json_result(browser::list_delmal(None).await, "Could not read delmål")
            }
            (None, None) => json_result(
                browser::list_competency_goals().await,
                "Could not read competency goals",
            ),
        }
    }

    #[tool(
        name = "get_half_year_tasks",
        description = "Read the six half-year assignment/minifagprøve areas, or one.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn get_half_year_tasks(
        &self,
        Parameters(request): Parameters<HalfYearTasksRequest>,
    ) -> CallToolResult {
        match request.ordinal {
            Some(ordinal) => json_result(
                browser::get_half_year_task(ordinal).await,
                "Could not read half-year task",
            ),
            None => json_result(
                browser::list_half_year_tasks().await,
                "Could not read half-year tasks",
            ),
        }
    }

    #[tool(
        name = "get_documentation",
        description = "List the user's documentation records, or read one in full (content, status, updated_at, targets).",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn get_documentation(
        &self,
        Parameters(request): Parameters<DocumentationRequest>,
    ) -> CallToolResult {
        match request.document_id {
            Some(document_id) => json_result(
                browser::get_documentation(&document_id).await,
                "Could not read documentation",
            ),
            None => json_result(
                browser::list_documentation(
                    request.page.unwrap_or(1),
                    request.page_size.unwrap_or(6),
                    request.query,
                    request.status,
                )
                .await,
                "Could not read documentation",
            ),
        }
    }

    #[tool(
        name = "get_feedback",
        description = "Read feedback on a documentation record, or one entry.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn get_feedback(
        &self,
        Parameters(request): Parameters<FeedbackRequest>,
    ) -> CallToolResult {
        match request.feedback_ordinal {
            Some(ordinal) => json_result(
                browser::get_feedback(&request.document_id, ordinal).await,
                "Could not read feedback",
            ),
            None => json_result(
                browser::list_feedback(&request.document_id).await,
                "Could not read feedback",
            ),
        }
    }

    #[tool(
        name = "list_attachments",
        description = "List attachments of a record, feedback entry, goal or delmål.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn list_attachments(
        &self,
        Parameters(request): Parameters<AttachmentListRequest>,
    ) -> CallToolResult {
        let source = match request.source.to_source() {
            Ok(source) => source,
            Err(error) => return json_error(error),
        };
        json_result(
            browser::list_attachments(source).await,
            "Could not read attachments",
        )
    }

    #[tool(
        name = "download_attachment",
        description = "Download an attachment to an absolute local path (max 25 MiB). Does not change Fagbrev.io.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn download_attachment(
        &self,
        Parameters(request): Parameters<AttachmentDownloadRequest>,
    ) -> CallToolResult {
        let source = match request.source.to_source() {
            Ok(source) => source,
            Err(error) => return json_error(error),
        };
        json_result(
            browser::download_attachment(
                source,
                request.attachment_ordinal,
                &request.output_path,
                request.overwrite,
            )
            .await,
            "Could not download attachment",
        )
    }

    #[tool(
        name = "find_reusable_content",
        description = "Suggest existing documentation whose text overlaps a goal/delmål target.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn find_reusable_content(
        &self,
        Parameters(request): Parameters<ReusableContentRequest>,
    ) -> CallToolResult {
        let target = match request.target.to_target() {
            Ok(target) => target,
            Err(error) => return json_error(error),
        };
        json_result(
            crate::reuse::find_reusable_content(
                target,
                request.query,
                request.source_documentation_ids,
                request.source_status,
                request.limit,
            )
            .await,
            "Could not find reusable content",
        )
    }

    #[tool(
        name = "make_documentation_template",
        description = "Build a documentation template for a target from selected records. Saves nothing.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn make_documentation_template(
        &self,
        Parameters(request): Parameters<DocumentationTemplateRequest>,
    ) -> CallToolResult {
        let target = match request.target.to_target() {
            Ok(target) => target,
            Err(error) => return json_error(error),
        };
        json_result(
            crate::reuse::make_documentation_template(
                target,
                request.title,
                request.source_documentation_ids,
                request.extra_instructions,
            )
            .await,
            "Could not make documentation template",
        )
    }

    #[tool(
        name = "save_draft",
        description = "Create or update a local-only draft. Never contacts Fagbrev.io.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn save_draft(
        &self,
        Parameters(request): Parameters<SaveDraftRequest>,
    ) -> CallToolResult {
        let target = match request.target.to_target() {
            Ok(target) => target,
            Err(error) => return json_error(error),
        };
        let store = match drafts::DraftStore::from_app_data() {
            Ok(store) => store,
            Err(error) => return json_error(error),
        };
        let updating = request.draft_id.is_some();
        let result = match request.draft_id {
            Some(draft_id) => {
                let Some(expected_version) = request.expected_version else {
                    return json_error(
                        "expected_version is required when save_draft updates an existing draft",
                    );
                };
                store.update(
                    &draft_id,
                    request.title,
                    request.content,
                    target,
                    request.source_documentation_ids,
                    expected_version,
                )
            }
            None => store.create(
                request.title,
                request.content,
                target,
                request.source_documentation_ids,
            ),
        };
        match result {
            Ok(draft) => json_success(&DraftWriteResult {
                outcome: if updating { "updated" } else { "created" }.to_string(),
                local_only: true,
                draft,
                message: "Saved locally; nothing was sent to Fagbrev.io.".to_string(),
            }),
            Err(error) => json_error(error),
        }
    }

    #[tool(
        name = "get_drafts",
        description = "List local drafts, or read one in full.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn get_drafts(&self, Parameters(request): Parameters<DraftsRequest>) -> CallToolResult {
        let store = match drafts::DraftStore::from_app_data() {
            Ok(store) => store,
            Err(error) => return json_error(error),
        };
        match request.draft_id {
            Some(draft_id) => match store.get(&draft_id) {
                Ok(draft) => json_success(&draft),
                Err(error) => json_error(error),
            },
            None => json_result(store.list(request.limit), "Could not list local drafts"),
        }
    }

    #[tool(
        name = "delete_draft",
        description = "Delete a local-only draft.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn delete_draft(&self, Parameters(request): Parameters<DraftRequest>) -> CallToolResult {
        match drafts::DraftStore::from_app_data().and_then(|store| {
            store.delete(&request.draft_id).map(|()| DraftDeleteResult {
                outcome: "deleted".to_string(),
                draft_id: request.draft_id,
                local_only: true,
                deleted: true,
                message: "Deleted the local draft; Fagbrev.io was not contacted.".to_string(),
            })
        }) {
            Ok(result) => json_success(&result),
            Err(error) => json_error(error),
        }
    }

    #[tool(
        name = "get_context_bundle",
        description = "One-call bounded snapshot: dashboard status, optional goal, and a few documentation records.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn get_context_bundle(
        &self,
        Parameters(request): Parameters<ContextBundleRequest>,
    ) -> CallToolResult {
        match context::build(context::ContextRequest {
            goal_number: request.goal_number,
            document_ids: request.document_ids,
            max_documents: request.max_documents,
            include_document_content: request.include_document_content,
        })
        .await
        {
            Ok(bundle) => json_success(&bundle),
            Err(error) => json_error(error),
        }
    }

    #[tool(
        name = "update_documentation",
        description = "Edit a documentation record (title, content, optional new target). Preview unless confirm=true.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn update_documentation(
        &self,
        Parameters(request): Parameters<UpdateDocumentationRequest>,
    ) -> CallToolResult {
        let preview = match update_preview(&request) {
            Ok(preview) => preview,
            Err(error) => return json_error(error),
        };

        if !request.confirm {
            return json_success(&UpdateDocumentationResult {
                outcome: "confirmation_required".to_string(),
                confirmation_required: true,
                mutated: false,
                preview,
                record: None,
                message: Some(UPDATE_WARNING.to_string()),
            });
        }

        match browser::update_documentation(
            &request.document_id,
            &request.title,
            &request.content,
            preview.replacement_target.as_ref(),
            &request.expected_updated_at,
            request.confirm,
        )
        .await
        {
            Ok(record) => json_success(&UpdateDocumentationResult {
                outcome: "updated".to_string(),
                confirmation_required: false,
                mutated: true,
                preview,
                record: Some(record),
                message: Some("The visible Lagre action was activated.".to_string()),
            }),
            Err(error) => json_error(format!(
                "Could not update documentation through the visible UI: {error:#}"
            )),
        }
    }

    #[tool(
        name = "delete_documentation",
        description = "Delete a draft documentation record via the visible UI. Preview unless confirm=true; may report unsupported.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn delete_documentation(
        &self,
        Parameters(request): Parameters<DeleteDocumentationRequest>,
    ) -> CallToolResult {
        let preview = match delete_preview(&request) {
            Ok(preview) => preview,
            Err(error) => return json_error(error),
        };

        if !request.confirm {
            return json_success(&preview);
        }

        json_result(
            browser::delete_documentation(
                &request.document_id,
                request.expected_status,
                &request.expected_updated_at,
                request.confirm,
            )
            .await,
            "Could not delete documentation through the visible UI",
        )
    }

    #[tool(
        name = "submit_documentation",
        description = "Create a documentation entry for a goal/delmål. Preview unless confirm=true (opening the form may create a blank draft).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn submit_documentation(
        &self,
        Parameters(request): Parameters<SubmitDocumentationRequest>,
    ) -> CallToolResult {
        let target = match request.target.to_target() {
            Ok(target) => target,
            Err(error) => return json_error(error),
        };
        let preview = match submit_preview(&request, &target) {
            Ok(preview) => preview,
            Err(error) => return json_error(error),
        };

        if !request.confirm {
            return json_success(&SubmitDocumentationResult {
                outcome: "confirmation_required".to_string(),
                confirmation_required: true,
                mutated: false,
                preview,
                record: None,
                message: Some(NEW_FORM_WARNING.to_string()),
            });
        }

        match browser::submit_documentation(
            &request.title,
            &request.content,
            &target,
            request.confirm,
        )
        .await
        {
            Ok(record) => json_success(&SubmitDocumentationResult {
                outcome: "saved".to_string(),
                confirmation_required: false,
                mutated: true,
                preview,
                record: Some(record),
                message: Some("The visible Lagre action was activated.".to_string()),
            }),
            Err(error) => json_error(format!(
                "Could not save documentation through the visible UI: {error:#}"
            )),
        }
    }

    #[tool(
        name = "request_approval",
        description = "Send a documentation record for approval. Preview unless confirm=true.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn request_approval(
        &self,
        Parameters(request): Parameters<RequestApprovalRequest>,
    ) -> CallToolResult {
        if let Err(error) = valid_documentation_id(&request.document_id) {
            return json_error(error);
        }

        if !request.confirm {
            return json_success(&approval_preview(request.document_id));
        }

        json_result(
            browser::request_approval(&request.document_id, request.confirm).await,
            "Could not request approval through the visible UI",
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl rmcp::ServerHandler for FagbrevServer {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new(
            "fagbrev",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(INSTRUCTIONS)
    }
}

pub async fn serve() -> Result<()> {
    let server = FagbrevServer::default();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        AttachmentDownloadRequest, AttachmentKind, AttachmentListRequest,
        DeleteDocumentationRequest, FagbrevServer, RequestApprovalRequest,
        SubmitDocumentationRequest, TargetArgs, UpdateDocumentationRequest, approval_preview,
        build_target, delete_preview, json_error, json_success, submit_preview, update_preview,
    };
    use crate::data::{AttachmentSource, DocumentationStatus};
    use rmcp::ServerHandler;
    use serde::Serializer;

    fn target_args() -> TargetArgs {
        TargetArgs {
            goal_number: 15,
            goal_title: Some("Feilsøke kode og rette feil".to_string()),
            delmal_number: Some(1),
            delmal_title: Some("Dokumentere virksomhetens rutiner for feilsøking".to_string()),
        }
    }

    fn update_request() -> UpdateDocumentationRequest {
        UpdateDocumentationRequest {
            document_id: "document-123".to_string(),
            title: "Updated title".to_string(),
            content: "Updated content".to_string(),
            expected_updated_at: "03.06.2026".to_string(),
            goal_number: None,
            goal_title: None,
            delmal_number: None,
            delmal_title: None,
            confirm: false,
        }
    }

    #[test]
    fn server_info_names_fagbrev_and_has_instructions() {
        let info = FagbrevServer::default().get_info();
        assert_eq!(info.server_info.name, "fagbrev");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(
            info.instructions
                .unwrap_or_default()
                .contains("confirm=true")
        );
    }

    #[test]
    fn every_tool_has_annotations_and_described_params() {
        let tools = FagbrevServer::default().tool_router.list_all();
        assert!(tools.len() <= 18, "expected a consolidated tool list");
        for tool in tools {
            let annotations = tool.annotations.as_ref().expect("annotations");
            assert!(annotations.read_only_hint.is_some(), "{}", tool.name);
            assert!(annotations.open_world_hint.is_some(), "{}", tool.name);
            let schema = serde_json::Value::Object((*tool.input_schema).clone());
            assert!(
                schema.get("$schema").is_none(),
                "{} not compacted",
                tool.name
            );
            assert!(!schema.to_string().contains("\"format\""), "{}", tool.name);
            assert!(
                schema.get("$defs").is_none(),
                "{} should not embed $defs",
                tool.name
            );
            if let Some(properties) = schema["properties"].as_object() {
                for (name, property) in properties {
                    assert!(
                        property.get("description").is_some(),
                        "{}.{name} lacks a description",
                        tool.name
                    );
                }
            }
        }
    }

    #[test]
    fn flat_target_builds_internal_target() {
        let target = target_args().to_target().expect("valid target");
        assert_eq!(target.competency_goal.as_ref().unwrap().number, 15);
        assert_eq!(target.delmal.as_ref().unwrap().number, Some(1));

        let delmal_only = TargetArgs {
            goal_title: None,
            ..target_args()
        }
        .to_target()
        .expect("delmål-only target");
        assert!(delmal_only.competency_goal.is_none());

        let goal_only = build_target(Some(3), None, None, None)
            .expect("goal-only")
            .expect("target present");
        assert_eq!(goal_only.competency_goal.unwrap().number, 3);
        assert!(goal_only.delmal.is_none());

        assert!(build_target(None, None, None, None).unwrap().is_none());
        assert!(build_target(Some(3), None, Some(1), None).is_err());
        assert!(build_target(Some(22), None, None, None).is_err());
        assert!(build_target(None, None, Some(1), Some("x".into())).is_err());
    }

    #[test]
    fn json_results_mark_errors_without_changing_success_text() {
        let success = json_success(&serde_json::json!({"count": 2}));
        assert_eq!(success.is_error, Some(false));
        assert_eq!(
            success.content[0].as_text().expect("text content").text,
            "{\n  \"count\": 2\n}"
        );

        let error = json_error("could not read dashboard");
        assert_eq!(error.is_error, Some(true));
        assert_eq!(
            error.content[0].as_text().expect("text content").text,
            "could not read dashboard"
        );
    }

    #[test]
    fn attachment_requests_use_flat_source_fields() {
        let request: AttachmentDownloadRequest = serde_json::from_value(serde_json::json!({
            "source": "feedback",
            "document_id": "doc-1",
            "feedback_ordinal": 1,
            "attachment_ordinal": 2,
            "output_path": "/tmp/file.pdf"
        }))
        .expect("download request should deserialize");
        assert_eq!(request.source.source, AttachmentKind::Feedback);
        assert_eq!(
            request.source.to_source().expect("valid source"),
            AttachmentSource::Feedback {
                document_id: "doc-1".to_string(),
                feedback_ordinal: 1
            }
        );
        assert!(!request.overwrite);

        let missing: AttachmentListRequest =
            serde_json::from_value(serde_json::json!({"source": "delmal", "goal_number": 2}))
                .expect("list request should deserialize");
        assert!(missing.source.to_source().is_err());
    }

    #[test]
    fn serialization_failures_are_mcp_errors() {
        struct FailingSerialization;

        impl serde::Serialize for FailingSerialization {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                Err(serde::ser::Error::custom(
                    "intentional serialization failure",
                ))
            }
        }

        let result = json_success(&FailingSerialization);
        assert_eq!(result.is_error, Some(true));
        assert!(
            result.content[0]
                .as_text()
                .expect("text content")
                .text
                .contains("intentional serialization failure")
        );
    }

    #[test]
    fn submit_preview_is_confirmation_gated_and_serializable() {
        let request: SubmitDocumentationRequest = serde_json::from_value(serde_json::json!({
            "title": "Feilsøking",
            "content": "Jeg undersøkte og dokumenterte feilen.",
            "goal_number": 15,
            "delmal_number": 1,
            "delmal_title": "Dokumentere virksomhetens rutiner for feilsøking"
        }))
        .expect("flat submit request should deserialize");
        assert!(!request.confirm);
        let target = request.target.to_target().expect("valid target");

        let preview = submit_preview(&request, &target).expect("valid write request");
        assert!(preview.confirmation_required);
        assert!(preview.would_mutate);
        assert!(preview.warning.contains("blank draft"));
        let encoded = serde_json::to_value(&preview).expect("preview should serialize");
        assert_eq!(encoded["target"]["delmal"]["goal_number"], 15);
    }

    #[test]
    fn approval_preview_never_reports_mutation() {
        let response = approval_preview("document-123".to_string());
        assert!(response.confirmation_required);
        assert!(response.would_mutate);
        assert!(!response.mutated);
        assert_eq!(response.document_id, "document-123");
        let encoded = serde_json::to_value(response).expect("approval preview should serialize");
        assert_eq!(encoded["outcome"], "confirmation_required");
    }

    #[test]
    fn approval_request_defaults_to_preview() {
        let request: RequestApprovalRequest =
            serde_json::from_str(r#"{"document_id":"document-123"}"#)
                .expect("request should deserialize");
        assert!(!request.confirm);
    }

    #[test]
    fn update_preview_requires_concurrency_guard_and_preserves_target_omission() {
        let request = update_request();
        let preview = update_preview(&request).expect("valid update request");
        assert!(preview.confirmation_required);
        assert!(preview.would_mutate);
        assert!(preview.replacement_target.is_none());
        assert!(preview.warning.contains("expected_updated_at"));

        let mut missing_guard = request;
        missing_guard.expected_updated_at.clear();
        let error = update_preview(&missing_guard).expect_err("guard is mandatory");
        assert!(error.to_string().contains("expected_updated_at"));
    }

    #[test]
    fn update_preview_validates_replacement_target_without_browser_access() {
        let args = target_args();
        let request = UpdateDocumentationRequest {
            goal_number: Some(args.goal_number),
            goal_title: args.goal_title.clone(),
            delmal_number: args.delmal_number,
            delmal_title: args.delmal_title.clone(),
            ..update_request()
        };

        let preview = update_preview(&request).expect("target should validate");
        assert_eq!(preview.replacement_target, Some(args.to_target().unwrap()));

        let invalid = UpdateDocumentationRequest {
            delmal_number: Some(1),
            ..update_request()
        };
        assert!(update_preview(&invalid).is_err());
    }

    #[test]
    fn delete_preview_is_draft_only_and_requires_explicit_version() {
        let request = DeleteDocumentationRequest {
            document_id: "document-123".to_string(),
            expected_status: DocumentationStatus::Draft,
            expected_updated_at: "11.09.2026".to_string(),
            confirm: false,
        };
        let preview = delete_preview(&request).expect("valid draft deletion request");
        assert_eq!(preview.outcome, "confirmation_required");
        assert!(preview.would_mutate);
        assert!(!preview.mutated);
        assert!(preview.message.contains("nothing changes"));

        let mut approved = request.clone();
        approved.expected_status = DocumentationStatus::Approved;
        let error = delete_preview(&approved).expect_err("approved records are protected");
        assert!(error.to_string().contains("expected_status=draft"));

        let mut missing_version = request;
        missing_version.expected_updated_at.clear();
        let error = delete_preview(&missing_version).expect_err("version is mandatory");
        assert!(error.to_string().contains("expected_updated_at"));
    }
}
