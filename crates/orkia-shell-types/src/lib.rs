// Copyright 2026 Orkia
// SPDX-License-Identifier: Elastic-2.0
//
// This file is part of the public Orkia shell. Licensed under the
// Elastic License 2.0; see the top-level LICENSE file
// for terms.

#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
// `cfg(test)` covers in-crate tests. The `test-utils` feature exposes
// the `mock` module to downstream test code, which uses `unwrap` on
// Mutex locks intentionally — a test mock can panic, that is its
// whole contract. Allow the same lint relaxation under that feature.
#![cfg_attr(
    any(test, feature = "test-utils"),
    allow(clippy::unwrap_used, clippy::expect_used)
)]

pub mod agent;
pub mod agent_def;
pub mod attached;
pub mod attention;
pub mod backend;
pub mod builtin_flags;
pub mod classifier;
pub mod decision;
pub mod dispatch_kernel;
pub mod error;
pub mod exec;
pub mod extensions;
pub mod forge_builder;
pub mod forge_kernel;
pub mod history;
pub mod input_limits;
pub mod job;
pub mod job_config;
pub mod journal;
pub mod kernel;
pub mod native;
pub mod pipeline_kernel;
pub mod policy;
pub mod provider;
pub mod renderer;
pub mod router;
pub mod scope;
pub mod seal;
pub mod seal_assembler;
pub mod seal_kernel;
pub mod team;
pub mod trust;
pub mod workspace;

pub use agent::{AgentInfo, AgentStatus};
pub use agent_def::{
    AgentConfigFile, AgentContextSection, AgentDefinition, AgentProjectsSection, AgentRuntimeKind,
    AgentRuntimeSection, AgentSection, AgentToolEntry, AgentToolsFile, AgentTrustSection,
    McpServerEntry, RuntimeKindError,
};
pub use attached::{AttachedHandle, AttachedOutcome, LivenessProbe};
pub use attention::{
    AttentionAction, AttentionCommandResult, AttentionControl, AttentionHint, AttentionId,
    AttentionKind, AttentionResolveEffect, AttentionRow, AttentionSeverity,
};
pub use builtin_flags::PsFlags;
pub use classifier::{IntentClassifier, IntentGuess};
pub use decision::{
    ApprovalStatus, BlockContent, CellStyle, Decision, Mode, NoOpReason, Outcome, PipelineStage,
    StyledCell,
};
// `METHOD_*` stay on the `dispatch_kernel::` path — re-exporting them here
// would collide with the identically named pipeline_kernel constants.
pub use dispatch_kernel::{
    DispatchAbortRequest, DispatchAbortResponse, DispatchAdvanceRequest, DispatchAdvanceResponse,
    DispatchAuthorizeRequest, DispatchAuthorizeResponse, DispatchTaskRequest, TaskOutcome,
    TaskOutputRef, TaskPlan,
};
pub use error::ShellError;
pub use exec::{
    CapabilityScope, CapabilitySet, Command, CommandCtx, EvaluatedCall, ExecError, ExecPlan,
    FlagSpec, ParsedStage, PipelineData, PositionalArg, Signature, Type, Value,
};
// `exec::Scope` (capability scope entry) is intentionally not re-exported at the
// crate root — it would collide with the workspace-scope `Scope` (below). Reach
pub use extensions::{
    AgentPipelineCoordinator, AgentPipelineRequest, AgentPipelineStage, AuthView, DaemonJobView,
    DaemonJobs, DaemonStageView, DetachedCageWrapper, DetachedSpawnRequest, DetachedSpawner,
    FinalResponseCallback, FinalResponseEvent, FinalResponseSource, JobEventObserver,
    JournalEnvelopeHook, JournalStopHook, PipelineDispatchOutcome, PipelineProgressCallback,
    PipelineProgressEvent,
};
pub use forge_builder::{BuildOutcome, BuilderError, ForgeBuilder, RecentBuild, UsageReport};
pub use forge_kernel::{
    ForgeBuildRequest, ForgeBuildResponse, ForgeUsageRequest, ForgeUsageResponse, ForgeWireError,
    METHOD_FORGE_BUILD, METHOD_FORGE_USAGE,
};
pub use history::{HistoryEntry, HistoryType};
pub use job::{
    JobEvent, JobId, JobInfo, JobKind, JobOwner, JobState, ParsedJobTarget, parse_job_target,
    render_job_id,
};
pub use job_config::{ProcessGroupMode, StdinSource};
pub use journal::{EventType, JournalEnvelope, JournalFilter};
pub use kernel::{
    KernelBenchmarkOutcome, KernelCancelOutcome, KernelContributeOutcome, KernelContributeStatus,
    KernelEvictOutcome, KernelModelStatus, KernelPullOutcome, KernelRpc, KernelRpcError,
    KernelVersion,
};
pub use native::{
    METHOD_LLM_COMPLETE, NativeChatMessage, NativeCompletionRequest, NativeCompletionResponse,
    NativeContentBlock, NativeFinish, NativeToolDef, NativeUsage,
};
pub use pipeline_kernel::{
    METHOD_ABORT, METHOD_ADVANCE, METHOD_AUTHORIZE, PipelineAbortRequest, PipelineAbortResponse,
    PipelineAdvanceRequest, PipelineAdvanceResponse, PipelineAuthorizeRequest,
    PipelineAuthorizeResponse, PipelineStageRequest, StageOutputRef, StagePlan,
};
pub use policy::{
    Adjustable, AskOutcome, Capability, ClassCaps, Policy, PolicyContext, PolicyDecision,
    PolicyError, PolicyProvider, Sensitivity, Verdict, WorkspaceScope,
};
pub use provider::{ProviderId, RuntimeCapabilities};
pub use renderer::{PromptContext, RenderEvent, RfcScopeSegment, ShellRenderer, WelcomeInfo};
pub use router::{AgentRouter, RoutingDecision, RoutingReason};
pub use scope::{
    IllegalOverride, Scope, ScopeParseError, resolve_effective_scope, validate_override,
};
pub use seal::{SealError, SealRecord};
pub use seal_assembler::{
    AssembleRequest, AssembleResult, ClosureReason, RfcSealAssembler, SealAssemblerError,
    VerifyOutcome,
};
pub use seal_kernel::{
    METHOD_SEAL_ASSEMBLE, METHOD_SEAL_VERIFY, SealAssembleResponse, SealVerifyRequest,
    SealVerifyResponse,
};
pub use team::{
    AcceptInviteOutcome, AddMemberArgs, CreateInviteArgs, CreateTeamArgs, InviteSummary,
    MeTeamMembership, MeView, NoopTeamClient, ProjectSummary, ShareIssueArgs, ShareProjectArgs,
    SharedProjectSummary, TeamClient, TeamClientError, TeamJoinResponse, TeamMemberSummary,
    TeamSnapshot, TeamSummary, WorkspaceMemberSummary, error_message as team_error_message,
};
pub use trust::{
    NoopTrustAdjuster, PendingStore, ProjectId, TrustAdjuster, TrustKey, TrustScope, UnlockStore,
    apply_trust, resolve_project_id,
};
pub use workspace::{
    IssueSummary, Project, RfcFrontmatter, RfcSummary, Workspace, parse_rfc_frontmatter,
};
