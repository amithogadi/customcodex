use super::*;
use crate::McpBinding;
use crate::client_tool_catalog::ClientToolCatalog;
use crate::elicitation::ElicitationLifecycle;
use crate::elicitation::ElicitationRequestManager;
use crate::elicitation::ElicitationRequestRouter;
use crate::elicitation::ElicitationReviewRequest;
use crate::elicitation::ElicitationReviewer;
use crate::elicitation::elicitation_is_rejected_by_policy;
use crate::mcp::tests::test_elicitation_config;
use crate::rmcp_client::AsyncManagedClient;
use crate::rmcp_client::ManagedClient;
use crate::rmcp_client::ManagedClientFuture;
use crate::rmcp_client::StartupOutcomeError;
use crate::rmcp_client::list_tools_for_client_uncached;
use crate::runtime::McpRuntimeContext;
use crate::server::EffectiveMcpServer;
use crate::server::McpServerMetadata;
use crate::server::McpServerOrigin;
use crate::tool_catalog_cache::McpToolCatalogCache;
use crate::tools::ToolFilter;
use crate::tools::ToolInfo;
use crate::tools::filter_tools;
use crate::tools::normalize_tools_for_model_with_prefix;
use assert_matches::assert_matches;
use codex_config::AppToolApproval;
use codex_config::Constrained;
use codex_config::McpServerAuth;
use codex_config::McpServerConfig;
use codex_config::McpServerEnvVar;
use codex_config::McpServerToolConfig;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_connectors::ConnectorRuntimeContext;
use codex_connectors::ConnectorRuntimeContextKey;
use codex_connectors::ConnectorRuntimeFetchSource;
use codex_connectors::ConnectorRuntimeManager;
use codex_exec_server::ExecServerError;
use codex_exec_server::HttpClient;
use codex_exec_server::HttpRequestParams;
use codex_exec_server::HttpRequestResponse;
use codex_exec_server::HttpResponseBodyStream;
use codex_exec_server_test_support::environment_manager_without_environments;
use codex_login::AuthHeaders;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::ToolName;
use codex_protocol::approvals::ElicitationRequest;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::mcp::McpServerInfo;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::GranularApprovalConfig;
use codex_protocol::protocol::McpStartupFailureReason;
use codex_rmcp_client::ElicitationResponse;
use codex_rmcp_client::InProcessTransportFactory;
use codex_rmcp_client::McpAuthState;
use codex_rmcp_client::McpLoginRequirement;
use codex_rmcp_client::McpOAuthRefreshMode;
use codex_rmcp_client::RmcpClient;
use codex_utils_path_uri::PathUri;
use futures::FutureExt;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use rmcp::ErrorData as McpError;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use rmcp::ServiceExt;
use rmcp::model::ClientCapabilities;
use rmcp::model::ElicitRequestParams;
use rmcp::model::ElicitationAction;
use rmcp::model::ElicitationCapability;
use rmcp::model::Implementation;
use rmcp::model::InitializeRequestParams;
use rmcp::model::JsonObject;
use rmcp::model::ListToolsResult;
use rmcp::model::NumberOrString;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ProtocolVersion;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use rmcp::service::RequestContext;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::tempdir;
use tokio::io::DuplexStream;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

impl McpConnectionSet {
    fn new_uninitialized(
        approval_policy: &Constrained<AskForApproval>,
        permission_profile: &Constrained<PermissionProfile>,
        prefix_mcp_tool_names: bool,
    ) -> Self {
        Self {
            servers: HashMap::new(),

            disabled_servers: Vec::new(),
            required_servers: Vec::new(),
            optional_startup_deadline: OnceLock::new(),
            tool_plugin_context: Arc::new(ToolPluginContext::default()),
            prefix_mcp_tool_names,
            non_prefixed_mcp_tool_servers: Vec::new(),
            elicitation_requests: ElicitationRequestManager::new(
                test_elicitation_config(
                    "server",
                    approval_policy.value(),
                    permission_profile.get().clone(),
                ),
                /*reviewer*/ None,
                /*lifecycle*/ None,
                ElicitationRequestRouter::default(),
            ),
        }
    }

    pub(crate) fn insert_test_client(
        &mut self,
        name: impl Into<String>,
        client: AsyncManagedClient,
    ) {
        let name = name.into();
        self.servers.insert(
            name,
            McpServerView {
                tool_filter: ToolFilter::default(),
                protocol_mode: crate::McpProtocolMode::Legacy,
                startup_readiness: Default::default(),
                connection: Arc::new(McpServerConnection {
                    identity: None,
                    client,
                    startup_timeout: DEFAULT_STARTUP_TIMEOUT,
                    startup_trigger: None,
                    _diagnostics_guard: LIVE_CONNECTIONS.track(),
                }),
                metadata: McpServerMetadata {
                    environment_id: String::new(),
                    pollutes_memory: true,
                    origin: None,
                    supports_parallel_tool_calls: false,
                    default_tools_approval_mode: None,
                    tool_approval_modes: HashMap::new(),
                },
                tool_timeout: None,
                catalog_item_limit: crate::pagination::MAX_MCP_CATALOG_ITEMS,
            },
        );
    }

    fn test_client(&self, name: &str) -> &AsyncManagedClient {
        &self.servers[name].connection.client
    }

    fn set_test_server_metadata(&mut self, name: &str, metadata: McpServerMetadata) {
        self.servers
            .get_mut(name)
            .expect("test server exists")
            .metadata = metadata;
    }

    fn shares_test_connection_with(&self, other: &Self, name: &str) -> bool {
        let Some(left) = self.servers.get(name) else {
            return false;
        };
        let Some(right) = other.servers.get(name) else {
            return false;
        };
        Arc::ptr_eq(&left.connection, &right.connection)
    }
}

pub(crate) fn create_test_tool(server_name: &str, tool_name: &str) -> ToolInfo {
    ToolInfo {
        server_name: server_name.to_string(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: tool_name.to_string(),
        callable_namespace: server_name.to_string(),
        namespace_description: None,
        tool: Tool::new(
            tool_name.to_string(),
            format!("Test tool: {tool_name}"),
            Arc::new(JsonObject::default()),
        ),

        connector_id: None,
        connector_name: None,
        plugin_display_names: Vec::new(),
    }
}

async fn capture_binding(manager: &Arc<McpConnectionSet>) -> McpBinding {
    let mut config = crate::mcp::tests::test_mcp_config(std::env::temp_dir());
    config.server_permission_profiles = manager
        .servers
        .keys()
        .map(|name| (name.clone(), PermissionProfile::default()))
        .collect();
    manager
        .capture_binding_with_metadata(
            Arc::new(config),
            /*plugins_available*/ false,
            /*required_servers*/ &[],
            /*required_plugins*/ &HashSet::new(),
        )
        .await
}

fn create_test_server_info(title: &str) -> McpServerInfo {
    McpServerInfo {
        name: "codex-apps".to_string(),
        title: Some(title.to_string()),
        version: "1.0.0".to_string(),
        description: None,
        icons: None,
        website_url: None,
    }
}

struct TestInProcessTransportFactory;

struct PendingHttpClient;

impl HttpClient for PendingHttpClient {
    fn http_request(
        &self,
        _params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
        futures::future::pending().boxed()
    }

    fn http_request_stream(
        &self,
        _params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
        futures::future::pending().boxed()
    }
}

impl InProcessTransportFactory for TestInProcessTransportFactory {
    fn open(&self) -> BoxFuture<'static, io::Result<DuplexStream>> {
        async {
            let (client_stream, _server_stream) = tokio::io::duplex(1);
            Ok(client_stream)
        }
        .boxed()
    }
}

#[derive(Clone)]
struct RefreshTestTransportFactory {
    tool: Tool,
    list_started: Option<Arc<Notify>>,
    release_list: Option<Arc<Notify>>,
    next_cursor: Option<String>,
    list_requests: Arc<AtomicUsize>,
}

impl ServerHandler for RefreshTestTransportFactory {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.list_requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(list_started) = &self.list_started {
            list_started.notify_one();
        }
        if let Some(release_list) = &self.release_list {
            release_list.notified().await;
        }
        let mut result = ListToolsResult::with_all_items(vec![self.tool.clone()]);
        result.next_cursor = self.next_cursor.clone();
        Ok(result)
    }
}

impl InProcessTransportFactory for RefreshTestTransportFactory {
    fn open(&self) -> BoxFuture<'static, io::Result<DuplexStream>> {
        let server = self.clone();
        async move {
            let (client_stream, server_stream) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let server = server
                    .serve(server_stream)
                    .await
                    .expect("serve test MCP server");
                server.waiting().await.expect("wait for test MCP server");
            });
            Ok(client_stream)
        }
        .boxed()
    }
}

#[derive(Clone)]
struct MutableToolsServer {
    tools: Arc<tokio::sync::RwLock<Vec<Tool>>>,
    block_tool_listing: Arc<AtomicBool>,
}

impl ServerHandler for MutableToolsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        if self.block_tool_listing.load(Ordering::Acquire) {
            std::future::pending::<()>().await;
        }
        Ok(ListToolsResult {
            tools: self.tools.read().await.clone(),
            ..Default::default()
        })
    }
}

struct MutableToolsTransportFactory {
    server: MutableToolsServer,
}

impl InProcessTransportFactory for MutableToolsTransportFactory {
    fn open(&self) -> BoxFuture<'static, io::Result<DuplexStream>> {
        let server = self.server.clone();
        async move {
            let (client_stream, server_stream) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                server
                    .serve(server_stream)
                    .await
                    .expect("serve mutable MCP tools")
                    .waiting()
                    .await
                    .expect("mutable MCP tools server completes");
            });
            Ok(client_stream)
        }
        .boxed()
    }
}

struct DisconnectingToolsTransportFactory {
    server: MutableToolsServer,
    disconnect: CancellationToken,
}

impl InProcessTransportFactory for DisconnectingToolsTransportFactory {
    fn open(&self) -> BoxFuture<'static, io::Result<DuplexStream>> {
        let server = self.server.clone();
        let disconnect = self.disconnect.clone();
        async move {
            let (client_stream, server_stream) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let server = server
                    .serve(server_stream)
                    .await
                    .expect("serve disconnecting MCP tools");
                let cancellation = server.cancellation_token();
                tokio::select! {
                    () = disconnect.cancelled() => cancellation.cancel(),
                    result = server.waiting() => {
                        result.expect("disconnecting MCP server should complete");
                    }
                }
            });
            Ok(client_stream)
        }
        .boxed()
    }
}

#[tokio::test]
async fn legacy_tool_catalog_rejects_repeated_pagination_cursor() -> anyhow::Result<()> {
    let requests = Arc::new(AtomicUsize::new(0));
    let client = Arc::new(
        RmcpClient::new_in_process_client(Arc::new(RefreshTestTransportFactory {
            tool: create_test_tool("legacy", "first-page").tool,
            list_started: None,
            release_list: None,
            next_cursor: Some("next-page".to_string()),
            list_requests: Arc::clone(&requests),
        }))
        .await?,
    );
    client
        .initialize(
            InitializeRequestParams::new(
                ClientCapabilities::default(),
                Implementation::new("codex-test", "0.0.0-test"),
            )
            .with_protocol_version(ProtocolVersion::V_2025_06_18),
            Some(Duration::from_secs(5)),
            Box::new(|_, _| async { Err(anyhow!("unexpected elicitation")) }.boxed()),
        )
        .await?;

    let error = list_tools_for_client_uncached(
        "legacy",
        &client,
        Some(Duration::from_secs(5)),
        crate::pagination::MAX_MCP_CATALOG_ITEMS,
        /*server_instructions*/ None,
    )
    .await
    .expect_err("repeated pagination cursor must fail discovery");

    assert_eq!(
        error.to_string(),
        "tools/list returned a repeated pagination cursor"
    );
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 2);
    client.shutdown().await;
    Ok(())
}

async fn create_test_managed_client(tools: Vec<ToolInfo>) -> ManagedClient {
    ManagedClient {
        _auth_change_notifications: None,
        client: Arc::new(
            RmcpClient::new_in_process_client(Arc::new(TestInProcessTransportFactory))
                .await
                .expect("create in-process RMCP client"),
        ),
        server_info: create_test_server_info("Ready"),
        tool_catalog: Arc::new(ClientToolCatalog::new(tools, /*updates*/ None)),
        tool_timeout: None,
        server_instructions: None,
        server_supports_sandbox_state_meta_capability: false,
    }
}

pub(crate) async fn create_ready_async_managed_client(tools: Vec<ToolInfo>) -> AsyncManagedClient {
    AsyncManagedClient {
        client: futures::future::ready::<Result<ManagedClient, StartupOutcomeError>>(Ok(
            create_test_managed_client(tools).await,
        ))
        .boxed()
        .shared(),

        server_capabilities: Arc::new(std::sync::Mutex::new(None)),

        tool_catalog_cache_context: None,
        startup_complete: Arc::new(std::sync::atomic::AtomicBool::new(true)),

        cancel_token: CancellationToken::new(),
    }
}

#[tokio::test]
async fn connection_statuses_observe_clients_without_starting_them() {
    use codex_protocol::mcp::McpServerConnectionStatus as Status;

    let mut manager = McpConnectionSet::empty(/*prefix_mcp_tool_names*/ true);
    manager.disabled_servers.push("disabled".to_string());
    let ready = create_ready_async_managed_client(Vec::new()).await;
    ready.client().await.expect("ready client");
    manager.insert_test_client("connected", ready);
    for (name, error) in [
        (
            "failed",
            StartupOutcomeError::Failed {
                error: "broken".to_string(),
                is_authentication_required: false,
            },
        ),
        (
            "auth",
            StartupOutcomeError::Failed {
                error: "login".to_string(),
                is_authentication_required: true,
            },
        ),
        (
            "flattened-auth",
            StartupOutcomeError::from(anyhow!("Auth required for server")),
        ),
        ("cancelled", StartupOutcomeError::Cancelled),
    ] {
        let mut client = create_ready_async_managed_client(Vec::new()).await;
        client.client = futures::future::ready(Err(error)).boxed().shared();
        assert!(client.client().await.is_err());
        manager.insert_test_client(name, client);
    }
    let mut pending = create_ready_async_managed_client(Vec::new()).await;
    pending.client = futures::future::pending().boxed().shared();
    manager.insert_test_client("starting", pending.clone());
    manager.insert_test_client("deferred", pending);
    let (trigger, _receiver) = watch::channel(/*init*/ false);
    Arc::get_mut(&mut manager.servers.get_mut("deferred").unwrap().connection)
        .unwrap()
        .startup_trigger = Some(trigger.clone());

    let statuses = tokio::time::timeout(
        Duration::from_millis(/*millis*/ 100),
        manager.connection_statuses(),
    )
    .await
    .expect("status must not await startup");
    let mut expected = HashMap::from([
        ("connected".to_string(), Status::Connected),
        ("failed".to_string(), Status::Failed),
        ("auth".to_string(), Status::AuthenticationRequired),
        ("flattened-auth".to_string(), Status::AuthenticationRequired),
        ("cancelled".to_string(), Status::Cancelled),
        ("starting".to_string(), Status::Starting),
        ("deferred".to_string(), Status::NotStarted),
        ("disabled".to_string(), Status::Disabled),
    ]);
    assert_eq!(statuses, expected);
    assert!(!*trigger.borrow());
    manager.test_client("connected").cancel_token.cancel();
    expected.insert("connected".to_string(), Status::Cancelled);
    assert_eq!(manager.connection_statuses().await, expected);
}

fn create_gated_async_managed_client(
    client: ManagedClient,
) -> (
    AsyncManagedClient,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let startup_complete = Arc::new(AtomicBool::new(false));
    let startup_complete_for_client = Arc::clone(&startup_complete);
    let client = async move {
        started_tx.send(()).expect("signal client startup");
        release_rx.await.expect("release client startup");
        startup_complete_for_client.store(true, std::sync::atomic::Ordering::Release);
        Ok(client)
    }
    .boxed()
    .shared();

    (
        AsyncManagedClient {
            client,

            server_capabilities: Arc::new(std::sync::Mutex::new(None)),

            tool_catalog_cache_context: None,
            startup_complete,

            cancel_token: CancellationToken::new(),
        },
        started_rx,
        release_tx,
    )
}

fn model_tool_names(tools: &[ToolInfo]) -> HashSet<ToolName> {
    tools
        .iter()
        .map(ToolInfo::canonical_tool_name)
        .collect::<HashSet<_>>()
}

fn model_tool_name_len(name: &ToolName) -> usize {
    name.namespace
        .as_deref()
        .map_or(0, |namespace| namespace.len() + "__".len())
        + name.name.len()
}

fn is_code_mode_compatible_tool_name(name: &ToolName) -> bool {
    name.namespace
        .as_deref()
        .into_iter()
        .chain(std::iter::once(name.name.as_str()))
        .flat_map(str::chars)
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[test]
fn elicitation_granular_policy_defaults_to_prompting() {
    assert!(!elicitation_is_rejected_by_policy(
        AskForApproval::OnRequest
    ));
    assert!(!elicitation_is_rejected_by_policy(
        AskForApproval::UnlessTrusted
    ));
    assert!(elicitation_is_rejected_by_policy(AskForApproval::Granular(
        GranularApprovalConfig {
            sandbox_approval: true,
            rules: true,
            skill_approval: true,
            request_permissions: true,
            mcp_elicitations: false,
        }
    )));
}

#[test]
fn elicitation_granular_policy_respects_never_and_config() {
    assert!(elicitation_is_rejected_by_policy(AskForApproval::Never));
    assert!(elicitation_is_rejected_by_policy(AskForApproval::Granular(
        GranularApprovalConfig {
            sandbox_approval: true,
            rules: true,
            skill_approval: true,
            request_permissions: true,
            mcp_elicitations: false,
        }
    )));
}

#[tokio::test]
async fn disabled_permissions_auto_accept_elicitation_with_empty_form_schema() {
    let manager = ElicitationRequestManager::new(
        test_elicitation_config("server", AskForApproval::Never, PermissionProfile::Disabled),
        /*reviewer*/ None,
        /*lifecycle*/ None,
        ElicitationRequestRouter::default(),
    );
    let (tx_event, _rx_event) = async_channel::bounded(1);
    let sender = manager.make_sender(
        "server".to_string(),
        Some(tx_event),
        &ClientMcpExtensions::default(),
    );

    let response = sender(
        NumberOrString::Number(1),
        codex_rmcp_client::Elicitation::Mcp(ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: "Confirm?".to_string(),
            requested_schema: rmcp::model::ElicitationSchema::builder()
                .build()
                .expect("schema should build"),
        }),
    )
    .await
    .expect("elicitation should auto accept");

    assert_eq!(
        response,
        ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(serde_json::json!({})),
            meta: None,
        }
    );
}

#[tokio::test]
async fn disabled_permissions_do_not_auto_accept_elicitation_with_requested_fields() {
    let manager = ElicitationRequestManager::new(
        test_elicitation_config("server", AskForApproval::Never, PermissionProfile::Disabled),
        /*reviewer*/ None,
        /*lifecycle*/ None,
        ElicitationRequestRouter::default(),
    );
    let (tx_event, _rx_event) = async_channel::bounded(1);
    let sender = manager.make_sender(
        "server".to_string(),
        Some(tx_event),
        &ClientMcpExtensions::default(),
    );

    let response = sender(
        NumberOrString::Number(1),
        codex_rmcp_client::Elicitation::Mcp(ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: "What should I say?".to_string(),
            requested_schema:
                rmcp::model::ElicitationSchema::builder()
                    .required_property(
                        "message",
                        rmcp::model::PrimitiveSchemaDefinition::String(
                            rmcp::model::StringSchema::new(),
                        ),
                    )
                    .build()
                    .expect("schema should build"),
        }),
    )
    .await
    .expect("elicitation should auto decline");

    assert_eq!(
        response,
        ElicitationResponse {
            action: ElicitationAction::Decline,
            content: None,
            meta: None,
        }
    );
}

fn full_access_form_input_enabled_router() -> ElicitationRequestRouter {
    let router = ElicitationRequestRouter::default();
    router.enable_full_access_form_input();
    router
}

fn elicitation_meta(value: serde_json::Value) -> Option<rmcp::model::RequestMetaObject> {
    let serde_json::Value::Object(map) = value else {
        panic!("elicitation metadata must be an object");
    };
    Some(rmcp::model::RequestMetaObject::from(map))
}

fn requested_user_input_schema() -> rmcp::model::ElicitationSchema {
    rmcp::model::ElicitationSchema::builder()
        .required_property(
            "message",
            rmcp::model::PrimitiveSchemaDefinition::String(rmcp::model::StringSchema::new()),
        )
        .build()
        .expect("schema should build")
}

#[derive(Default)]
struct DecliningElicitationReviewer {
    review_count: AtomicUsize,
}

impl ElicitationReviewer for DecliningElicitationReviewer {
    fn review(
        &self,
        _request: ElicitationReviewRequest,
    ) -> BoxFuture<'static, anyhow::Result<Option<ElicitationResponse>>> {
        self.review_count.fetch_add(1, Ordering::SeqCst);
        async {
            Ok(Some(ElicitationResponse {
                action: ElicitationAction::Decline,
                content: None,
                meta: None,
            }))
        }
        .boxed()
    }
}

async fn assert_elicitation_declined_with_reviewer_calls(
    approval_policy: AskForApproval,
    server_name: &str,
    elicitation: ElicitRequestParams,
    expected_reviewer_calls: usize,
) {
    let reviewer = Arc::new(DecliningElicitationReviewer::default());
    let manager = ElicitationRequestManager::new(
        test_elicitation_config(server_name, approval_policy, PermissionProfile::Disabled),
        Some(reviewer.clone()),
        /*lifecycle*/ None,
        full_access_form_input_enabled_router(),
    );
    let (tx_event, rx_event) = async_channel::bounded(1);
    let sender = manager.make_sender(
        server_name.to_string(),
        Some(tx_event),
        &ClientMcpExtensions::default(),
    );

    let response = tokio::select! {
        biased;
        event = rx_event.recv() => {
            panic!("elicitation unexpectedly reached the user: {event:?}");
        }
        response = sender(
            NumberOrString::Number(1),
            codex_rmcp_client::Elicitation::Mcp(elicitation),
        ) => response.expect("elicitation should be declined"),
    };

    assert_eq!(
        response,
        ElicitationResponse {
            action: ElicitationAction::Decline,
            content: None,
            meta: None,
        },
    );
    assert_eq!(
        reviewer.review_count.load(Ordering::SeqCst),
        expected_reviewer_calls
    );
    assert!(rx_event.try_recv().is_err());
}

async fn assert_requested_user_input_is_declined(
    approval_policy: AskForApproval,
    permission_profile: PermissionProfile,
    router: ElicitationRequestRouter,
) {
    let manager = ElicitationRequestManager::new(
        test_elicitation_config("server", approval_policy, permission_profile),
        /*reviewer*/ None,
        /*lifecycle*/ None,
        router,
    );
    let (tx_event, rx_event) = async_channel::bounded(1);
    let sender = manager.make_sender(
        "server".to_string(),
        Some(tx_event),
        &ClientMcpExtensions::default(),
    );

    let response = tokio::select! {
        biased;
        event = rx_event.recv() => {
            panic!("user-input form unexpectedly reached the user: {event:?}");
        }
        response = sender(
            NumberOrString::Number(1),
            codex_rmcp_client::Elicitation::Mcp(
                ElicitRequestParams::FormElicitationParams {
                    meta: None,
                    message: "What should I say?".to_string(),
                    requested_schema: requested_user_input_schema(),
                },
            ),
        ) => response.expect("restricted user-input request should decline"),
    };

    assert_eq!(
        response,
        ElicitationResponse {
            action: ElicitationAction::Decline,
            content: None,
            meta: None,
        },
    );
    assert!(rx_event.try_recv().is_err());
}

#[tokio::test]
async fn disabled_permissions_do_not_surface_user_input_when_auto_denied() {
    let router = full_access_form_input_enabled_router();
    router.set_auto_deny(/*auto_deny*/ true);
    assert_requested_user_input_is_declined(
        AskForApproval::Never,
        PermissionProfile::Disabled,
        router,
    )
    .await;
}

#[tokio::test]
async fn plugin_tool_suggestion_elicitations_are_declined_before_review() {
    assert_elicitation_declined_with_reviewer_calls(
        AskForApproval::OnRequest,
        "server",
        ElicitRequestParams::FormElicitationParams {
            meta: elicitation_meta(serde_json::json!({
                "codex_approval_kind": "tool_suggestion",
            })),
            message: "Install this app?".to_string(),
            requested_schema: rmcp::model::ElicitationSchema::builder()
                .build()
                .expect("schema should build"),
        },
        /*expected_reviewer_calls*/ 0,
    )
    .await;
}

#[tokio::test]
async fn disabled_permissions_surface_requested_user_input_without_metadata() {
    assert_disabled_permissions_surface_requested_user_input(/*meta*/ None).await;
}

#[tokio::test]
async fn disabled_permissions_surface_requested_user_input_with_non_codex_approval_metadata() {
    assert_disabled_permissions_surface_requested_user_input(elicitation_meta(serde_json::json!({
        "origin": "https://example.com",
        "persist": "always",
    })))
    .await;
}

async fn assert_disabled_permissions_surface_requested_user_input(
    meta: Option<rmcp::model::RequestMetaObject>,
) {
    let router = full_access_form_input_enabled_router();
    let reviewer = Arc::new(DecliningElicitationReviewer::default());
    let manager = ElicitationRequestManager::new(
        test_elicitation_config("server", AskForApproval::Never, PermissionProfile::Disabled),
        Some(reviewer.clone()),
        /*lifecycle*/ None,
        router.clone(),
    );
    let (tx_event, rx_event) = async_channel::bounded(1);
    let sender = manager.make_sender(
        "server".to_string(),
        Some(tx_event),
        &ClientMcpExtensions::default(),
    );
    let requested_schema = requested_user_input_schema();
    let mut pending = tokio::spawn(sender(
        NumberOrString::Number(1),
        codex_rmcp_client::Elicitation::Mcp(ElicitRequestParams::FormElicitationParams {
            meta: meta.clone(),
            message: "What should I say?".to_string(),
            requested_schema: requested_schema.clone(),
        }),
    ));
    let request = tokio::select! {
        event = rx_event.recv() => {
            let EventMsg::ElicitationRequest(request) = event.expect("user-input event").msg else {
                panic!("expected MCP user-input elicitation");
            };
            request
        }
        response = &mut pending => {
            panic!("user input resolved without reaching the user: {response:?}");
        }
    };

    assert_eq!(
        request.request,
        ElicitationRequest::Form {
            meta: meta
                .map(serde_json::to_value)
                .transpose()
                .expect("user-input metadata should serialize"),
            message: "What should I say?".to_string(),
            requested_schema: serde_json::to_value(requested_schema)
                .expect("schema should serialize"),
        },
    );
    assert_eq!(request.server_name, "server");
    assert_eq!(reviewer.review_count.load(Ordering::SeqCst), 0);

    let codex_protocol::mcp::RequestId::String(request_id) = request.id else {
        panic!("expected Codex-owned string request ID");
    };
    let user_response = ElicitationResponse {
        action: ElicitationAction::Accept,
        content: Some(serde_json::json!({ "message": "The actual user response." })),
        meta: None,
    };
    router
        .resolve(
            "server".to_string(),
            NumberOrString::String(request_id.into()),
            user_response.clone(),
        )
        .await
        .expect("actual user response should resolve the elicitation");
    assert_eq!(
        pending
            .await
            .expect("user-input task should complete")
            .expect("user input should resolve"),
        user_response,
    );
}

#[tokio::test]
async fn disabled_permissions_decline_requested_user_input_with_approval_metadata() {
    assert_elicitation_declined_with_reviewer_calls(
        AskForApproval::Never,
        "node_repl",
        ElicitRequestParams::FormElicitationParams {
            meta: elicitation_meta(serde_json::json!({
                "codex_approval_kind": "mcp_tool_call",
                "connector_id": "browser-use",
                "tool_name": "access_browser_origin",
            })),
            message: "Allow Browser Use to access this website?".to_string(),
            requested_schema:
                rmcp::model::ElicitationSchema::builder()
                    .required_property(
                        "confirmation",
                        rmcp::model::PrimitiveSchemaDefinition::String(
                            rmcp::model::StringSchema::new(),
                        ),
                    )
                    .build()
                    .expect("schema should build"),
        },
        /*expected_reviewer_calls*/ 0,
    )
    .await;
}

#[tokio::test]
async fn restricted_never_policy_does_not_surface_requested_user_input() {
    assert_requested_user_input_is_declined(
        AskForApproval::Never,
        PermissionProfile::default(),
        full_access_form_input_enabled_router(),
    )
    .await;
}

#[tokio::test]
async fn granular_policy_does_not_surface_requested_user_input() {
    assert_requested_user_input_is_declined(
        AskForApproval::Granular(GranularApprovalConfig {
            sandbox_approval: true,
            rules: true,
            skill_approval: true,
            request_permissions: true,
            mcp_elicitations: false,
        }),
        PermissionProfile::Disabled,
        full_access_form_input_enabled_router(),
    )
    .await;
}

#[tokio::test]
async fn on_request_approval_forms_remain_with_the_reviewer() {
    assert_elicitation_declined_with_reviewer_calls(
        AskForApproval::OnRequest,
        "server",
        ElicitRequestParams::FormElicitationParams {
            meta: elicitation_meta(serde_json::json!({
                "codex_request_type": "approval_request",
                "codex_approval_kind": "mcp_tool_call",
                "tool_name": "test_tool",
            })),
            message: "Approve this action?".to_string(),
            requested_schema: rmcp::model::ElicitationSchema::builder()
                .build()
                .expect("schema should build"),
        },
        /*expected_reviewer_calls*/ 1,
    )
    .await;
}

#[tokio::test]
async fn disabled_permissions_decline_user_input_without_an_event_channel() {
    let manager = ElicitationRequestManager::new(
        test_elicitation_config("server", AskForApproval::Never, PermissionProfile::Disabled),
        /*reviewer*/ None,
        /*lifecycle*/ None,
        full_access_form_input_enabled_router(),
    );
    let sender = manager.make_sender(
        "server".to_string(),
        /*tx_event*/ None,
        &ClientMcpExtensions::default(),
    );

    let response = sender(
        NumberOrString::Number(1),
        codex_rmcp_client::Elicitation::Mcp(ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: "What should I say?".to_string(),
            requested_schema: requested_user_input_schema(),
        }),
    )
    .await
    .expect("headless user-input request should decline");

    assert_eq!(
        response,
        ElicitationResponse {
            action: ElicitationAction::Decline,
            content: None,
            meta: None,
        },
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_authority_updates_never_auto_approve_mixed_policy() {
    let manager = ElicitationRequestManager::new(
        test_elicitation_config(
            "server",
            AskForApproval::Never,
            PermissionProfile::default(),
        ),
        /*reviewer*/ None,
        /*lifecycle*/ None,
        ElicitationRequestRouter::default(),
    );
    let updating_manager = manager.clone();
    let updater = tokio::spawn(async move {
        for _ in 0..1_000 {
            assert!(updating_manager.update(
                test_elicitation_config(
                    "server",
                    AskForApproval::OnRequest,
                    PermissionProfile::Disabled
                ),
                /*reviewer*/ None,
                /*lifecycle*/ None,
            ));
            assert!(updating_manager.update(
                test_elicitation_config(
                    "server",
                    AskForApproval::Never,
                    PermissionProfile::default()
                ),
                /*reviewer*/ None,
                /*lifecycle*/ None,
            ));
        }
    });
    let sender = manager.make_sender(
        "server".to_string(),
        /*tx_event*/ None,
        &ClientMcpExtensions::default(),
    );
    let elicitation =
        codex_rmcp_client::Elicitation::Mcp(ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: "Confirm?".to_string(),
            requested_schema: rmcp::model::ElicitationSchema::builder()
                .build()
                .expect("schema should build"),
        });

    for _ in 0..1_000 {
        let response = sender(NumberOrString::Number(1), elicitation.clone())
            .await
            .expect("elicitation should resolve");
        assert_eq!(
            response,
            ElicitationResponse {
                action: ElicitationAction::Decline,
                content: None,
                meta: None,
            }
        );
    }

    updater.await.expect("authority updates should finish");
}

#[tokio::test]
async fn shared_elicitation_router_targets_the_exact_pending_request() {
    struct Registration(Arc<AtomicUsize>);

    impl Drop for Registration {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let router = ElicitationRequestRouter::default();
    let outstanding = Arc::new(AtomicUsize::new(0));
    let lifecycle = ElicitationLifecycle::new({
        let outstanding = outstanding.clone();
        move || {
            outstanding.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Registration(outstanding.clone())
        }
    });
    let manager_a = ElicitationRequestManager::new(
        test_elicitation_config(
            "server",
            AskForApproval::OnRequest,
            PermissionProfile::default(),
        ),
        /*reviewer*/ None,
        Some(lifecycle.clone()),
        router.clone(),
    );
    let manager_b = ElicitationRequestManager::new(
        test_elicitation_config(
            "server",
            AskForApproval::OnRequest,
            PermissionProfile::default(),
        ),
        /*reviewer*/ None,
        Some(lifecycle),
        router.clone(),
    );
    let (tx_event, rx_event) = async_channel::bounded(2);
    let sender_a = manager_a.make_sender(
        "server".to_string(),
        Some(tx_event.clone()),
        &ClientMcpExtensions::default(),
    );
    let sender_b = manager_b.make_sender(
        "server".to_string(),
        Some(tx_event),
        &ClientMcpExtensions::default(),
    );
    let elicitation =
        codex_rmcp_client::Elicitation::Mcp(ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: "Which runtime?".to_string(),
            requested_schema:
                rmcp::model::ElicitationSchema::builder()
                    .required_property(
                        "runtime",
                        rmcp::model::PrimitiveSchemaDefinition::String(
                            rmcp::model::StringSchema::new(),
                        ),
                    )
                    .build()
                    .expect("schema should build"),
        });

    let pending_a = tokio::spawn(sender_a(NumberOrString::Number(1), elicitation.clone()));
    let EventMsg::ElicitationRequest(request_a) = rx_event.recv().await.expect("request A").msg
    else {
        panic!("expected elicitation request");
    };
    let pending_b = tokio::spawn(sender_b(NumberOrString::Number(1), elicitation));
    let EventMsg::ElicitationRequest(request_b) = rx_event.recv().await.expect("request B").msg
    else {
        panic!("expected elicitation request");
    };
    assert_eq!(outstanding.load(std::sync::atomic::Ordering::SeqCst), 2);
    let (
        codex_protocol::mcp::RequestId::String(request_a_id),
        codex_protocol::mcp::RequestId::String(request_b_id),
    ) = (request_a.id, request_b.id)
    else {
        panic!("expected Codex-owned string request IDs");
    };
    assert_ne!(request_a_id, request_b_id);

    let response_a = ElicitationResponse {
        action: ElicitationAction::Accept,
        content: Some(serde_json::json!({"runtime": "a"})),
        meta: None,
    };
    router
        .resolve(
            "server".to_string(),
            NumberOrString::String(request_a_id.into()),
            response_a.clone(),
        )
        .await
        .expect("runtime B should route a response to runtime A");
    let response_b = ElicitationResponse {
        action: ElicitationAction::Accept,
        content: Some(serde_json::json!({"runtime": "b"})),
        meta: None,
    };
    router
        .resolve(
            "server".to_string(),
            NumberOrString::String(request_b_id.into()),
            response_b.clone(),
        )
        .await
        .expect("runtime A should route a response to runtime B");

    assert_eq!(
        pending_a
            .await
            .expect("request A task")
            .expect("request A response"),
        response_a
    );
    assert_eq!(
        pending_b
            .await
            .expect("request B task")
            .expect("request B response"),
        response_b
    );
    assert_eq!(outstanding.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelled_elicitation_is_removed_without_affecting_other_pending_requests() {
    let router = ElicitationRequestRouter::default();
    let manager = ElicitationRequestManager::new(
        test_elicitation_config(
            "server",
            AskForApproval::OnRequest,
            PermissionProfile::default(),
        ),
        /*reviewer*/ None,
        /*lifecycle*/ None,
        router.clone(),
    );
    let (tx_event, rx_event) = async_channel::bounded(2);
    let sender = manager.make_sender(
        "server".to_string(),
        Some(tx_event),
        &ClientMcpExtensions::default(),
    );
    let elicitation =
        codex_rmcp_client::Elicitation::Mcp(ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: "Confirm?".to_string(),
            requested_schema:
                rmcp::model::ElicitationSchema::builder()
                    .required_property(
                        "answer",
                        rmcp::model::PrimitiveSchemaDefinition::String(
                            rmcp::model::StringSchema::new(),
                        ),
                    )
                    .build()
                    .expect("schema should build"),
        });

    let cancelled = tokio::spawn(sender(NumberOrString::Number(1), elicitation.clone()));
    let EventMsg::ElicitationRequest(cancelled_request) =
        rx_event.recv().await.expect("cancelled request event").msg
    else {
        panic!("expected elicitation request");
    };
    let pending = tokio::spawn(sender(NumberOrString::Number(2), elicitation));
    let EventMsg::ElicitationRequest(pending_request) =
        rx_event.recv().await.expect("pending request event").msg
    else {
        panic!("expected elicitation request");
    };
    let (
        codex_protocol::mcp::RequestId::String(cancelled_id),
        codex_protocol::mcp::RequestId::String(pending_id),
    ) = (cancelled_request.id, pending_request.id)
    else {
        panic!("expected Codex-owned string request IDs");
    };

    cancelled.abort();
    assert!(
        cancelled
            .await
            .expect_err("cancelled request should be aborted")
            .is_cancelled()
    );

    let response = ElicitationResponse {
        action: ElicitationAction::Accept,
        content: Some(serde_json::json!({"answer": "yes"})),
        meta: None,
    };
    let error = router
        .resolve(
            "server".to_string(),
            NumberOrString::String(cancelled_id.into()),
            response.clone(),
        )
        .await
        .expect_err("cancelled request should be removed immediately");
    assert_eq!(error.to_string(), "elicitation request not found");

    router
        .resolve(
            "server".to_string(),
            NumberOrString::String(pending_id.into()),
            response.clone(),
        )
        .await
        .expect("another pending request should remain routable");
    assert_eq!(
        pending
            .await
            .expect("pending request task")
            .expect("pending request response"),
        response
    );
}

#[test]
fn test_normalize_tools_short_non_duplicated_names() {
    let tools = vec![
        create_test_tool("server1", "tool1"),
        create_test_tool("server1", "tool2"),
    ];

    let model_tools =
        normalize_tools_for_model_with_prefix(tools, /*prefix_mcp_tool_names*/ true, &[]);

    assert_eq!(
        model_tool_names(&model_tools),
        HashSet::from([
            ToolName::namespaced("mcp__server1", "tool1"),
            ToolName::namespaced("mcp__server1", "tool2")
        ])
    );
}

#[test]
fn test_normalize_tools_omits_prefix_only_for_selected_servers() {
    let tools = vec![
        create_test_tool("history", "search"),
        create_test_tool("notes", "read"),
        create_test_tool("calendar", "list"),
    ];

    let model_tools = normalize_tools_for_model_with_prefix(
        tools,
        /*prefix_mcp_tool_names*/ true,
        &["history".to_string(), "notes".to_string()],
    );

    assert_eq!(
        model_tool_names(&model_tools),
        HashSet::from([
            ToolName::namespaced("history", "search"),
            ToolName::namespaced("notes", "read"),
            ToolName::namespaced("mcp__calendar", "list"),
        ])
    );
}

#[test]
fn test_normalize_tools_selects_raw_server_name() {
    let mut tool = create_test_tool("codex_apps", "search");
    tool.callable_namespace = "codex_apps__calendar".to_string();

    let model_tools = normalize_tools_for_model_with_prefix(
        vec![tool],
        /*prefix_mcp_tool_names*/ true,
        &["codex_apps".to_string()],
    );

    assert_eq!(
        model_tool_names(&model_tools),
        HashSet::from([ToolName::namespaced("codex_apps__calendar", "search")])
    );
}

#[test]
fn test_normalize_tools_global_feature_omits_prefix_for_every_server() {
    let tools = vec![
        create_test_tool("history", "search"),
        create_test_tool("calendar", "list"),
    ];

    let model_tools = normalize_tools_for_model_with_prefix(
        tools,
        /*prefix_mcp_tool_names*/ false,
        &["history".to_string()],
    );

    assert_eq!(
        model_tool_names(&model_tools),
        HashSet::from([
            ToolName::namespaced("history", "search"),
            ToolName::namespaced("calendar", "list"),
        ])
    );
}

#[test]
fn test_normalize_tools_duplicated_names_skipped() {
    let tools = vec![
        create_test_tool("server1", "duplicate_tool"),
        create_test_tool("server1", "duplicate_tool"),
    ];

    let model_tools =
        normalize_tools_for_model_with_prefix(tools, /*prefix_mcp_tool_names*/ true, &[]);

    // Only the first tool should remain, the second is skipped
    assert_eq!(
        model_tool_names(&model_tools),
        HashSet::from([ToolName::namespaced("mcp__server1", "duplicate_tool")])
    );
}

#[test]
fn test_normalize_tools_respects_responses_api_name_length_boundaries() {
    let namespace = "mcp__codex_apps";
    let namespace_len = namespace.len() + "__".len();

    for total_len in [128, 129] {
        let tool_name = "a".repeat(total_len - namespace_len);
        let model_tools = normalize_tools_for_model_with_prefix(
            vec![create_test_tool("codex_apps", &tool_name)],
            /*prefix_mcp_tool_names*/ true,
            &[],
        );
        let model_name = model_tools[0].canonical_tool_name();

        assert_eq!(model_tool_name_len(&model_name), 128);
        if total_len == 128 {
            assert_eq!(model_name, ToolName::namespaced(namespace, tool_name));
        } else {
            assert_ne!(model_name.name, tool_name);
        }
    }
}

#[test]
fn test_normalize_tools_long_names_same_server() {
    let server_name = "my_server";
    let first_name = "a".repeat(128);
    let second_name = "b".repeat(128);

    let tools = vec![
        create_test_tool(server_name, &first_name),
        create_test_tool(server_name, &second_name),
    ];

    let model_tools =
        normalize_tools_for_model_with_prefix(tools, /*prefix_mcp_tool_names*/ true, &[]);

    assert_eq!(model_tools.len(), 2);

    let names = model_tool_names(&model_tools);

    assert!(names.iter().all(|name| model_tool_name_len(name) == 128));
    assert!(
        names
            .iter()
            .all(|name| name.namespace.as_deref() == Some("mcp__my_server"))
    );
    assert!(
        names.iter().all(is_code_mode_compatible_tool_name),
        "model-visible names must be code-mode compatible: {names:?}"
    );
}

#[test]
fn test_normalize_tools_sanitizes_invalid_characters() {
    let tools = vec![create_test_tool("server.one", "tool.two-three")];

    let model_tools =
        normalize_tools_for_model_with_prefix(tools, /*prefix_mcp_tool_names*/ true, &[]);

    assert_eq!(model_tools.len(), 1);
    let tool = model_tools.into_iter().next().expect("one tool");
    let model_name = tool.canonical_tool_name();
    assert_eq!(
        model_name,
        ToolName::namespaced("mcp__server_one", "tool_two_three")
    );
    assert_eq!(
        ToolName::namespaced(tool.callable_namespace.clone(), tool.callable_name.clone()),
        model_name
    );
    // The callable parts are sanitized for model-visible tool calls, but the raw
    // MCP name is preserved for the actual MCP call.
    assert_eq!(tool.server_name, "server.one");
    assert_eq!(tool.callable_namespace, "mcp__server_one");
    assert_eq!(tool.callable_name, "tool_two_three");
    assert_eq!(tool.tool.name, "tool.two-three");

    assert!(
        is_code_mode_compatible_tool_name(&model_name),
        "model-visible name must be code-mode compatible: {model_name:?}"
    );
}

#[test]
fn test_normalize_tools_keeps_hyphenated_mcp_tools_callable() {
    let tools = vec![create_test_tool("music-studio", "get-strudel-guide")];

    let model_tools =
        normalize_tools_for_model_with_prefix(tools, /*prefix_mcp_tool_names*/ true, &[]);

    assert_eq!(model_tools.len(), 1);
    let tool = model_tools.into_iter().next().expect("one tool");
    assert_eq!(
        tool.canonical_tool_name(),
        ToolName::namespaced("mcp__music_studio", "get_strudel_guide")
    );
    assert_eq!(tool.callable_namespace, "mcp__music_studio");
    assert_eq!(tool.callable_name, "get_strudel_guide");
    assert_eq!(tool.tool.name, "get-strudel-guide");
}

#[test]
fn test_normalize_tools_disambiguates_sanitized_namespace_collisions() {
    let tools = vec![
        create_test_tool("basic-server", "lookup"),
        create_test_tool("basic_server", "query"),
        create_test_tool("npm:@scope/package.name", "lookup"),
        create_test_tool("npm__scope_package_name", "lookup"),
    ];

    let model_tools =
        normalize_tools_for_model_with_prefix(tools, /*prefix_mcp_tool_names*/ true, &[]);

    assert_eq!(model_tools.len(), 4);
    let mut namespaces = model_tools
        .iter()
        .map(|tool| tool.callable_namespace.as_str())
        .collect::<Vec<_>>();
    namespaces.sort();
    namespaces.dedup();
    assert_eq!(namespaces.len(), 4);

    let raw_servers = model_tools
        .iter()
        .map(|tool| tool.server_name.as_str())
        .collect::<HashSet<_>>();
    assert_eq!(
        raw_servers,
        HashSet::from([
            "basic-server",
            "basic_server",
            "npm:@scope/package.name",
            "npm__scope_package_name",
        ])
    );
    let model_names = model_tool_names(&model_tools);
    assert!(
        model_names.iter().all(is_code_mode_compatible_tool_name),
        "model-visible names must be code-mode compatible: {model_names:?}"
    );
}

#[test]
fn test_normalize_tools_disambiguates_sanitized_tool_name_collisions() {
    let tools = vec![
        create_test_tool("server", "tool-name"),
        create_test_tool("server", "tool_name"),
    ];

    let model_tools =
        normalize_tools_for_model_with_prefix(tools, /*prefix_mcp_tool_names*/ true, &[]);

    assert_eq!(model_tools.len(), 2);
    let raw_tool_names = model_tools
        .iter()
        .map(|tool| tool.tool.name.to_string())
        .collect::<HashSet<_>>();
    assert_eq!(
        raw_tool_names,
        HashSet::from(["tool-name".to_string(), "tool_name".to_string()])
    );
    let callable_tool_names = model_tools
        .iter()
        .map(|tool| tool.callable_name.as_str())
        .collect::<HashSet<_>>();
    assert_eq!(callable_tool_names.len(), 2);
}

#[test]
fn tool_filter_allows_by_default() {
    let filter = ToolFilter::default();

    assert!(filter.allows("any"));
}

#[test]
fn tool_filter_applies_enabled_list() {
    let filter = ToolFilter {
        enabled: Some(HashSet::from(["allowed".to_string()])),
        disabled: HashSet::new(),
    };

    assert!(filter.allows("allowed"));
    assert!(!filter.allows("denied"));
}

#[test]
fn tool_filter_applies_disabled_list() {
    let filter = ToolFilter {
        enabled: None,
        disabled: HashSet::from(["blocked".to_string()]),
    };

    assert!(!filter.allows("blocked"));
    assert!(filter.allows("open"));
}

#[test]
fn tool_filter_applies_enabled_then_disabled() {
    let filter = ToolFilter {
        enabled: Some(HashSet::from(["keep".to_string(), "remove".to_string()])),
        disabled: HashSet::from(["remove".to_string()]),
    };

    assert!(filter.allows("keep"));
    assert!(!filter.allows("remove"));
    assert!(!filter.allows("unknown"));
}

#[test]
fn filter_tools_applies_per_server_filters() {
    let server1_tools = vec![
        create_test_tool("server1", "tool_a"),
        create_test_tool("server1", "tool_b"),
    ];
    let server2_tools = vec![create_test_tool("server2", "tool_a")];
    let server1_filter = ToolFilter {
        enabled: Some(HashSet::from(["tool_a".to_string(), "tool_b".to_string()])),
        disabled: HashSet::from(["tool_b".to_string()]),
    };
    let server2_filter = ToolFilter {
        enabled: None,
        disabled: HashSet::from(["tool_a".to_string()]),
    };

    let filtered: Vec<_> = filter_tools(server1_tools, &server1_filter)
        .into_iter()
        .chain(filter_tools(server2_tools, &server2_filter))
        .collect();

    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].server_name, "server1");
    assert_eq!(filtered[0].callable_name, "tool_a");
}

#[tokio::test(start_paused = true)]
async fn tool_catalog_cache_sanitizes_tools_and_tracks_environment_generation() {
    let cache = McpToolCatalogCache::default();
    let environment_manager = Arc::new(environment_manager_without_environments());
    let replace_environment = |url: &str| {
        environment_manager
            .upsert_environment(
                "remote".to_string(),
                url.to_string(),
                /*connect_timeout*/ None,
            )
            .expect("replace environment");
    };
    replace_environment("ws://127.0.0.1:1");
    let runtime_context =
        McpRuntimeContext::new(Arc::clone(&environment_manager), PathBuf::from("/tmp"));
    let config: McpServerConfig = serde_json::from_value(serde_json::json!({
        "command": "docs-mcp",
        "environment_id": "remote"
    }))
    .expect("MCP config");
    let resolve_environment = || {
        runtime_context
            .resolve_server_environment("docs", &config)
            .expect("resolve environment")
            .expect("remote environment")
    };
    let cache_context = |environment: &Arc<codex_exec_server::Environment>| {
        cache
            .context(
                "docs",
                &config,
                &runtime_context,
                Some(environment),
                (
                    &ElicitationCapability::default(),
                    &ClientMcpExtensions::default(),
                ),
                /*connection_identity*/ None,
            )
            .expect("cache context")
    };
    let first_environment = resolve_environment();
    let first_environment_weak = Arc::downgrade(&first_environment);
    let first_context = cache_context(&first_environment);
    first_context.publish_if_newest(first_context.begin_fetch(), &[]);
    assert!(!first_context.has_tools());

    let mut tool = create_test_tool("docs", "search");
    tool.tool.annotations = Some(rmcp::model::ToolAnnotations::new().read_only(true));
    first_context.publish_if_newest(first_context.begin_fetch(), &[tool]);
    assert_eq!(
        first_context.current_tools().expect("cached tools")[0]
            .tool
            .annotations,
        None
    );

    drop(first_environment);
    replace_environment("ws://127.0.0.1:2");
    assert!(first_environment_weak.upgrade().is_none());
    let replacement_environment = resolve_environment();
    assert!(!cache_context(&replacement_environment).has_tools());

    let older = first_context.begin_fetch();
    let newer = first_context.begin_fetch();
    first_context.publish_if_newest(newer, &[create_test_tool("docs", "new")]);
    first_context.publish_if_newest(older, &[create_test_tool("docs", "old")]);
    assert_eq!(
        first_context.current_tools().expect("cached tools")[0].callable_name,
        "new"
    );

    tokio::time::advance(Duration::from_secs(30 * 60 + 1)).await;
    assert!(!first_context.has_tools());
}

#[test]
fn tool_catalog_cache_bypasses_remote_sourced_environment_variables() {
    let cache = McpToolCatalogCache::default();
    let runtime_context = McpRuntimeContext::new(
        Arc::new(environment_manager_without_environments()),
        PathBuf::from("/tmp"),
    );
    let config: McpServerConfig = serde_json::from_value(serde_json::json!({
        "command": "docs-mcp",
        "env_vars": [McpServerEnvVar::Config {
            name: "DOCS_TOKEN".to_string(),
            source: Some("remote".to_string()),
        }],
    }))
    .expect("MCP config");

    assert!(
        cache
            .context(
                "docs",
                &config,
                &runtime_context,
                /*resolved_environment*/ None,
                (
                    &ElicitationCapability::default(),
                    &ClientMcpExtensions::default()
                ),
                /*connection_identity*/ None,
            )
            .is_none()
    );
}

#[test]
fn tool_catalog_cache_bypasses_http_headers_helpers() {
    let cache = McpToolCatalogCache::default();
    let runtime_context = reusable_server_runtime_context();
    let mut config = reusable_server_config("https://example.com/mcp");
    let identity = reusable_server_identity("docs", &config, &runtime_context);
    let context = |config: &McpServerConfig, identity: &McpServerConnectionIdentity| {
        cache.context(
            "docs",
            config,
            &runtime_context,
            /*resolved_environment*/ None,
            (
                &ElicitationCapability::default(),
                &ClientMcpExtensions::default(),
            ),
            Some((
                identity,
                crate::McpProtocolMode::Legacy,
                /*agent_plugin*/ false,
            )),
        )
    };
    assert!(context(&config, &identity).is_some());

    let McpServerTransportConfig::StreamableHttp {
        http_headers_helper,
        ..
    } = &mut config.transport
    else {
        unreachable!("expected HTTP transport");
    };
    *http_headers_helper = Some("auth-cli headers".to_string());
    let identity = reusable_server_identity("docs", &config, &runtime_context);
    assert!(context(&config, &identity).is_none());
}

#[tokio::test]
async fn list_available_server_infos_does_not_block_on_pending_client() {
    let pending_client = futures::future::pending::<Result<ManagedClient, StartupOutcomeError>>()
        .boxed()
        .shared();
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    manager.insert_test_client(
        CODEX_APPS_MCP_SERVER_NAME.to_string(),
        AsyncManagedClient {
            client: pending_client,

            server_capabilities: Arc::new(std::sync::Mutex::new(None)),

            tool_catalog_cache_context: None,
            startup_complete: Arc::new(std::sync::atomic::AtomicBool::new(false)),

            cancel_token: CancellationToken::new(),
        },
    );

    let timeout_result = tokio::time::timeout(
        Duration::from_millis(10),
        manager.list_available_server_infos(|_| true),
    )
    .await;
    let server_infos = timeout_result.expect("server info lookup should not block on startup");
    assert!(!server_infos.contains_key(CODEX_APPS_MCP_SERVER_NAME));
}

#[tokio::test]
async fn list_all_tools_accepts_canonical_namespaced_tool_names() {
    let managed_client =
        create_ready_async_managed_client(vec![create_test_tool("rmcp", "echo")]).await;
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ false,
    );
    manager.insert_test_client("rmcp", managed_client);

    let tools = manager.list_all_tools().await;
    let tool = tools
        .iter()
        .find(|tool| tool.canonical_tool_name() == ToolName::namespaced("rmcp", "echo"))
        .expect("split MCP tool namespace and name should resolve");

    let expected = ("rmcp", "rmcp", "echo", "echo");
    assert_eq!(
        (
            tool.server_name.as_str(),
            tool.callable_namespace.as_str(),
            tool.callable_name.as_str(),
            tool.tool.name.as_ref(),
        ),
        expected
    );
}

#[tokio::test(start_paused = true)]
async fn capture_binding_skips_pending_optional_servers_after_configured_shared_startup_grace() {
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    let mut plugin_config = crate::mcp::tests::test_mcp_config(std::env::temp_dir());
    let mut catalog = crate::ResolvedMcpCatalog::builder();
    catalog.register(crate::McpServerRegistration::from_plugin(
        "pending-one".to_string(),
        crate::McpPluginAttribution::new("optional-plugin".to_string(), "Optional".to_string()),
        /*plugin_order*/ 0,
        serde_json::from_value(serde_json::json!({ "command": "optional-plugin" }))
            .expect("optional plugin MCP config"),
    ));
    catalog.register(crate::McpServerRegistration::from_selected_plugin(
        "pending-selected".to_string(),
        crate::McpPluginAttribution::new("selected-plugin".to_string(), "Selected".to_string()),
        /*selection_order*/ 0,
        codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID,
        serde_json::from_value(serde_json::json!({ "command": "selected-plugin" }))
            .expect("selected plugin MCP config"),
    ));
    plugin_config.mcp_server_catalog = catalog.build();
    plugin_config.optional_mcp_startup_grace = Duration::from_millis(250);
    manager.tool_plugin_context = Arc::new(crate::tool_plugin_context(&plugin_config));
    for server_name in ["pending-one", "pending-two", "pending-selected"] {
        manager.insert_test_client(
            server_name.to_string(),
            AsyncManagedClient {
                client: futures::future::pending::<Result<ManagedClient, StartupOutcomeError>>()
                    .boxed()
                    .shared(),

                server_capabilities: Arc::new(std::sync::Mutex::new(None)),

                tool_catalog_cache_context: None,
                startup_complete: Arc::new(AtomicBool::new(false)),

                cancel_token: CancellationToken::new(),
            },
        );
    }

    let mut required_manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    required_manager.tool_plugin_context = Arc::clone(&manager.tool_plugin_context);
    required_manager.insert_test_client(
        "pending-selected",
        manager.test_client("pending-selected").clone(),
    );
    required_manager.required_servers = vec!["pending-selected".to_string()];

    let manager = Arc::new(manager);
    assert!(
        manager
            .stable_catalog_revisions(
                /*required_servers*/ &[],
                /*required_plugins*/ &HashSet::new()
            )
            .await
            .is_none()
    );
    let started = tokio::time::Instant::now();
    let binding = tokio::time::timeout(
        Duration::from_millis(500),
        manager.capture_binding_with_metadata(
            Arc::new(plugin_config),
            /*plugins_available*/ false,
            /*required_servers*/ &[],
            /*required_plugins*/ &HashSet::new(),
        ),
    )
    .await
    .expect("all optional servers should share the configured startup grace");
    assert!(binding.tools().is_empty());
    assert_eq!(started.elapsed(), Duration::from_millis(250));

    let binding = tokio::time::timeout(Duration::from_millis(1), capture_binding(&manager))
        .await
        .expect("later bindings must not restart the optional startup grace");
    assert!(binding.tools().is_empty());

    assert!(
        tokio::time::timeout(
            Duration::from_millis(1),
            binding.list_resources("pending-one", /*params*/ None),
        )
        .await
        .is_err(),
        "resources must wait for an omitted server instead of failing immediately"
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(1),
            binding.list_all_resources(|server| server == "pending-one"),
        )
        .await
        .is_ok(),
        "resource discovery must not wait for an omitted optional server"
    );

    for server_name in ["pending-one", "pending-selected"] {
        let required_servers = vec![server_name.to_string()];
        let binding = tokio::time::timeout(
            Duration::from_millis(1),
            manager.capture_binding_with_metadata(
                Arc::new(crate::mcp::tests::test_mcp_config(std::env::temp_dir())),
                /*plugins_available*/ false,
                &required_servers,
                /*required_plugins*/ &HashSet::new(),
            ),
        )
        .await;
        assert!(binding.is_err(), "explicitly requested servers must wait");
    }
    // A plugin mention must still require startup after the optional grace has elapsed.
    for (plugin_id, must_wait) in [
        ("selected-plugin", true),
        ("optional-plugin", false),
        ("selected-plugin-other", false),
    ] {
        let required_plugins = HashSet::from([plugin_id.to_string()]);
        let binding = tokio::time::timeout(
            Duration::from_millis(1),
            manager.capture_binding_with_metadata(
                Arc::new(crate::mcp::tests::test_mcp_config(std::env::temp_dir())),
                /*plugins_available*/ false,
                /*required_servers*/ &[],
                &required_plugins,
            ),
        )
        .await;
        assert_eq!(
            binding.is_err(),
            must_wait,
            "plugin requirement {plugin_id}"
        );
    }
    assert!(
        tokio::time::timeout(
            Duration::from_millis(1500),
            capture_binding(&Arc::new(required_manager)),
        )
        .await
        .is_err(),
        "configured-required selected plugin servers must wait beyond the optional grace"
    );
}

#[tokio::test(start_paused = true)]
async fn capture_binding_waits_for_optional_startup_when_shared_grace_is_disabled() {
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    let (client, startup_started, release_startup) = create_gated_async_managed_client(
        create_test_managed_client(vec![create_test_tool("optional", "echo")]).await,
    );
    manager.insert_test_client("optional", client);

    let mut config = crate::mcp::tests::test_mcp_config(std::env::temp_dir());
    config.optional_mcp_startup_grace = Duration::ZERO;
    config
        .server_permission_profiles
        .insert("optional".to_string(), PermissionProfile::default());
    let manager = Arc::new(manager);
    let mut capture = tokio::spawn(async move {
        manager
            .capture_binding_with_metadata(
                Arc::new(config),
                /*plugins_available*/ false,
                /*required_servers*/ &[],
                /*required_plugins*/ &HashSet::new(),
            )
            .await
    });

    startup_started.await.expect("client startup should begin");
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut capture)
            .await
            .is_err(),
        "disabled shared grace should keep waiting for optional startup"
    );
    release_startup.send(()).expect("release client startup");

    let binding = capture.await.expect("capture binding task");
    assert!(binding.prepare_call("optional", "echo").is_some());
}

#[tokio::test]
async fn stable_catalog_revisions_ignore_terminal_optional_server_failures() {
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    let ready = create_ready_async_managed_client(vec![create_test_tool("ready", "echo")]).await;
    assert!(ready.client().await.is_ok());
    let mut failed = ready.clone();
    manager.insert_test_client("ready", ready);
    failed.client = futures::future::ready::<Result<ManagedClient, StartupOutcomeError>>(Err(
        StartupOutcomeError::Failed {
            error: "optional startup failed".to_string(),
            is_authentication_required: false,
        },
    ))
    .boxed()
    .shared();
    assert!(failed.client().await.is_err());
    manager.insert_test_client("failed", failed);

    assert!(
        manager
            .stable_catalog_revisions(
                /*required_servers*/ &[],
                /*required_plugins*/ &HashSet::new()
            )
            .await
            .is_some()
    );
    manager.required_servers.push("failed".to_string());
    assert!(
        manager
            .stable_catalog_revisions(
                /*required_servers*/ &[],
                /*required_plugins*/ &HashSet::new()
            )
            .await
            .is_none()
    );
    manager.required_servers.clear();

    let binding = capture_binding(&Arc::new(manager)).await;
    assert!(binding.prepare_call("ready", "echo").is_some());
}

#[tokio::test(start_paused = true)]
async fn capture_binding_shares_optional_startup_grace_across_connection_sets() {
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let cache = McpToolCatalogCache::default();
    let runtime_context = McpRuntimeContext::new(
        Arc::new(environment_manager_without_environments()),
        std::env::temp_dir(),
    );
    let server_config: McpServerConfig =
        serde_json::from_value(serde_json::json!({ "command": "pending-mcp" }))
            .expect("pending MCP server configuration");
    let cache_context = cache
        .context(
            "pending",
            &server_config,
            &runtime_context,
            /*resolved_environment*/ None,
            (
                &ElicitationCapability::default(),
                &ClientMcpExtensions::default(),
            ),
            /*connection_identity*/ None,
        )
        .expect("shared pending MCP catalog");

    let create_connection_set = || {
        let mut manager = McpConnectionSet::new_uninitialized(
            &approval_policy,
            &permission_profile,
            /*prefix_mcp_tool_names*/ true,
        );
        manager.insert_test_client(
            "pending",
            AsyncManagedClient {
                client: futures::future::pending::<Result<ManagedClient, StartupOutcomeError>>()
                    .boxed()
                    .shared(),

                server_capabilities: Arc::new(std::sync::Mutex::new(None)),

                tool_catalog_cache_context: Some(cache_context.clone()),
                startup_complete: Arc::new(AtomicBool::new(false)),

                cancel_token: CancellationToken::new(),
            },
        );
        Arc::new(manager)
    };

    let first_started = tokio::time::Instant::now();
    let first = tokio::time::timeout(
        Duration::from_millis(1500),
        capture_binding(&create_connection_set()),
    )
    .await
    .expect("the first thread should receive the optional startup grace");
    assert!(first.tools().is_empty());
    assert_eq!(first_started.elapsed(), Duration::from_secs(1));

    let second = tokio::time::timeout(
        Duration::from_millis(1),
        capture_binding(&create_connection_set()),
    )
    .await
    .expect("the next thread must not restart the same server's startup grace");
    assert!(second.tools().is_empty());

    let mut disabled_config = crate::mcp::tests::test_mcp_config(std::env::temp_dir());
    disabled_config.optional_mcp_startup_grace = Duration::ZERO;
    let disabled_manager = create_connection_set();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(1),
            disabled_manager.capture_binding_with_metadata(
                Arc::new(disabled_config),
                /*plugins_available*/ false,
                /*required_servers*/ &[],
                /*required_plugins*/ &HashSet::new(),
            ),
        )
        .await
        .is_err(),
        "disabled grace should keep waiting for the pending optional server"
    );

    let restored_started = tokio::time::Instant::now();
    let restored = tokio::time::timeout(
        Duration::from_millis(1500),
        capture_binding(&create_connection_set()),
    )
    .await
    .expect("restoring the startup grace should create a fresh deadline");
    assert!(restored.tools().is_empty());
    assert_eq!(restored_started.elapsed(), Duration::from_secs(1));

    let mut updated_config = crate::mcp::tests::test_mcp_config(std::env::temp_dir());
    updated_config.optional_mcp_startup_grace = Duration::from_millis(250);
    let updated_manager = create_connection_set();
    let updated_started = tokio::time::Instant::now();
    let updated = tokio::time::timeout(
        Duration::from_millis(500),
        updated_manager.capture_binding_with_metadata(
            Arc::new(updated_config),
            /*plugins_available*/ false,
            /*required_servers*/ &[],
            /*required_plugins*/ &HashSet::new(),
        ),
    )
    .await
    .expect("a changed startup grace should receive its newly configured deadline");
    assert!(updated.tools().is_empty());
    assert_eq!(updated_started.elapsed(), Duration::from_millis(250));

    cache_context.publish_if_newest(
        cache_context.begin_fetch(),
        &[create_test_tool("pending", "cached_tool")],
    );
    let deadline_after_publication = tokio::time::Instant::now() + Duration::from_secs(1);
    assert_eq!(
        cache_context.optional_startup_deadline(deadline_after_publication, Duration::from_secs(1)),
        deadline_after_publication,
        "publishing a catalog must not install a stale startup deadline"
    );
    let cached_manager = create_connection_set();
    let cached = tokio::time::timeout(Duration::from_millis(1), capture_binding(&cached_manager))
        .await
        .expect("cached tools should be immediately available to later threads");
    assert_eq!(
        cached
            .tools()
            .iter()
            .map(|tool| tool.callable_name.as_str())
            .collect::<Vec<_>>(),
        vec!["cached_tool"]
    );

    tokio::time::advance(Duration::from_secs(30 * 60 + 1)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(1), capture_binding(&cached_manager))
            .await
            .is_err(),
        "an expired catalog should receive a fresh startup grace"
    );

    cache_context.disable();
    for _ in 0..2 {
        let started = tokio::time::Instant::now();
        let binding = tokio::time::timeout(
            Duration::from_millis(1500),
            capture_binding(&create_connection_set()),
        )
        .await
        .expect("non-cacheable servers should keep their per-thread startup grace");
        assert!(binding.tools().is_empty());
        assert_eq!(started.elapsed(), Duration::from_secs(1));
    }
}

#[tokio::test(start_paused = true)]
async fn capture_binding_uses_cache_published_during_optional_startup() {
    let runtime_context = McpRuntimeContext::new(
        Arc::new(environment_manager_without_environments()),
        std::env::temp_dir(),
    );
    let server_config: McpServerConfig =
        serde_json::from_value(serde_json::json!({ "command": "pending-mcp" }))
            .expect("server configuration");
    let cache_context = McpToolCatalogCache::default()
        .context(
            "pending",
            &server_config,
            &runtime_context,
            /*resolved_environment*/ None,
            (
                &ElicitationCapability::default(),
                &ClientMcpExtensions::default(),
            ),
            /*connection_identity*/ None,
        )
        .expect("shared catalog");
    let (mut client, started, _release) = create_gated_async_managed_client(
        create_test_managed_client(vec![create_test_tool("pending", "live_tool")]).await,
    );
    client.tool_catalog_cache_context = Some(cache_context.clone());
    let mut manager = McpConnectionSet::new_uninitialized(
        &Constrained::allow_any(AskForApproval::OnRequest),
        &Constrained::allow_any(PermissionProfile::default()),
        /*prefix_mcp_tool_names*/ true,
    );
    manager.insert_test_client("pending", client);
    let manager = Arc::new(manager);

    let binding = capture_binding(&manager);
    tokio::pin!(binding);
    assert!(futures::poll!(&mut binding).is_pending());
    started
        .await
        .expect("optional startup began without a cache");
    cache_context.publish_if_newest(
        cache_context.begin_fetch(),
        &[create_test_tool("pending", "peer_tool")],
    );
    tokio::time::advance(Duration::from_secs(/*secs*/ 2)).await;

    let binding = binding.await;
    assert!(
        !manager.servers["pending"]
            .connection
            .client
            .startup_complete
            .load(Ordering::Acquire)
    );
    assert_eq!(
        model_tool_names(binding.tools()),
        HashSet::from([ToolName::namespaced("mcp__pending", "peer_tool")]),
    );
}

#[tokio::test(start_paused = true)]
async fn capture_binding_retains_cached_tools_that_expire_while_waiting_for_another_server() {
    let runtime_context = McpRuntimeContext::new(
        Arc::new(environment_manager_without_environments()),
        std::env::temp_dir(),
    );
    let server_config: McpServerConfig =
        serde_json::from_value(serde_json::json!({ "command": "server-a-mcp" }))
            .expect("MCP server configuration");
    let cache_context = McpToolCatalogCache::default()
        .context(
            "server_a",
            &server_config,
            &runtime_context,
            /*resolved_environment*/ None,
            (
                &ElicitationCapability::default(),
                &ClientMcpExtensions::default(),
            ),
            /*connection_identity*/ None,
        )
        .expect("server A cache context");
    let (mut client_a, _started_a, _release_a) = create_gated_async_managed_client(
        create_test_managed_client(vec![create_test_tool("server_a", "live_tool")]).await,
    );
    client_a.tool_catalog_cache_context = Some(cache_context.clone());
    let (client_b, started_b, release_b) = create_gated_async_managed_client(
        create_test_managed_client(vec![create_test_tool("server_b", "tool_b")]).await,
    );
    cache_context.publish_if_newest(
        cache_context.begin_fetch(),
        &[create_test_tool("server_a", "cached_tool")],
    );
    tokio::time::advance(Duration::from_secs(30 * 60 - 1)).await;

    let mut manager = McpConnectionSet::new_uninitialized(
        &Constrained::allow_any(AskForApproval::OnRequest),
        &Constrained::allow_any(PermissionProfile::default()),
        /*prefix_mcp_tool_names*/ true,
    );
    manager.insert_test_client("server_a", client_a);
    manager.insert_test_client("server_b", client_b);
    Arc::get_mut(&mut manager.servers.get_mut("server_a").unwrap().connection)
        .expect("unique server A connection")
        .startup_trigger = Some(watch::channel(/*init*/ false).0);
    manager.required_servers = vec!["server_a".to_string(), "server_b".to_string()];
    let manager = Arc::new(manager);

    let binding = capture_binding(&manager);
    tokio::pin!(binding);
    assert!(futures::poll!(&mut binding).is_pending());
    started_b.await.expect("server B startup should begin");
    assert!(manager.servers["server_a"].connection.startup_is_dormant());

    // Expire A's catalog while capture is waiting for B's required startup.
    tokio::time::advance(Duration::from_secs(/*secs*/ 2)).await;
    assert!(cache_context.current_tools().is_none());
    release_b.send(()).expect("release server B startup");

    let binding = tokio::time::timeout(Duration::from_secs(/*secs*/ 1), binding)
        .await
        .expect("cached server A should not need startup");
    assert_eq!(
        model_tool_names(binding.tools()),
        HashSet::from([
            ToolName::namespaced("mcp__server_a", "cached_tool"),
            ToolName::namespaced("mcp__server_b", "tool_b"),
        ])
    );
    assert!(manager.servers["server_a"].connection.startup_is_dormant());
}

#[tokio::test]
async fn capture_binding_omits_cache_disabled_while_waiting_for_another_server() {
    let runtime_context = McpRuntimeContext::new(
        Arc::new(environment_manager_without_environments()),
        std::env::temp_dir(),
    );
    let server_config: McpServerConfig =
        serde_json::from_value(serde_json::json!({ "command": "cached-mcp" }))
            .expect("server configuration");
    let cache_context = McpToolCatalogCache::default()
        .context(
            "cached",
            &server_config,
            &runtime_context,
            /*resolved_environment*/ None,
            (
                &ElicitationCapability::default(),
                &ClientMcpExtensions::default(),
            ),
            /*connection_identity*/ None,
        )
        .expect("shared catalog");
    cache_context.publish_if_newest(
        cache_context.begin_fetch(),
        &[create_test_tool("cached", "cached_tool")],
    );
    let (mut cached, _started, _release) = create_gated_async_managed_client(
        create_test_managed_client(vec![create_test_tool("cached", "live_tool")]).await,
    );
    cached.tool_catalog_cache_context = Some(cache_context.clone());
    let (waiting, started, release) = create_gated_async_managed_client(
        create_test_managed_client(vec![create_test_tool("waiting", "ready_tool")]).await,
    );
    let mut manager = McpConnectionSet::new_uninitialized(
        &Constrained::allow_any(AskForApproval::OnRequest),
        &Constrained::allow_any(PermissionProfile::default()),
        /*prefix_mcp_tool_names*/ true,
    );
    manager.insert_test_client("cached", cached);
    manager.insert_test_client("waiting", waiting);
    manager.required_servers = vec!["waiting".to_string()];
    let manager = Arc::new(manager);

    let binding = capture_binding(&manager);
    tokio::pin!(binding);
    assert!(futures::poll!(&mut binding).is_pending());
    started.await.expect("required server startup began");
    cache_context.disable();
    release.send(()).expect("release required server startup");

    assert_eq!(
        model_tool_names(binding.await.tools()),
        HashSet::from([ToolName::namespaced("mcp__waiting", "ready_tool")]),
    );
}

#[tokio::test]
async fn list_all_tools_applies_legacy_mcp_prefix_by_default() {
    let managed_client =
        create_ready_async_managed_client(vec![create_test_tool("rmcp", "echo")]).await;
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    manager.insert_test_client("rmcp", managed_client);

    let tools = manager.list_all_tools().await;
    let tool = tools
        .iter()
        .find(|tool| tool.canonical_tool_name() == ToolName::namespaced("mcp__rmcp", "echo"))
        .expect("legacy-prefixed MCP tool name should resolve");

    let expected = ("rmcp", "mcp__rmcp", "echo", "echo");
    assert_eq!(
        (
            tool.server_name.as_str(),
            tool.callable_namespace.as_str(),
            tool.callable_name.as_str(),
            tool.tool.name.as_ref(),
        ),
        expected
    );
}

#[tokio::test]
async fn call_tool_requires_connection_without_waiting_for_startup() {
    let client = create_test_managed_client(vec![create_test_tool("docs", "search")]).await;
    let (client, startup_started, release_startup) = create_gated_async_managed_client(client);
    let startup_client = client.clone();
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    manager.insert_test_client("docs", client);

    let pending_call = tokio::time::timeout(
        Duration::from_millis(50),
        manager.call_tool(
            "docs",
            "search",
            /*environment_id*/ None,
            /*arguments*/ None,
            /*meta*/ None,
            Some(Duration::from_secs(5)),
            /*wait_for_server*/ false,
        ),
    )
    .await
    .expect("ready-only invocation must not wait for pending server startup")
    .expect_err("pending server must not accept ready-only calls");
    assert!(pending_call.to_string().contains("not connected"));

    let startup = tokio::spawn(async move { startup_client.client().await });
    startup_started.await.expect("server startup should begin");
    release_startup.send(()).expect("release server startup");
    startup
        .await
        .expect("startup task should finish")
        .expect("server startup should succeed");

    let ready_error = manager
        .call_tool(
            "docs",
            "search",
            /*environment_id*/ None,
            /*arguments*/ None,
            /*meta*/ None,
            Some(Duration::from_secs(5)),
            /*wait_for_server*/ false,
        )
        .await
        .expect_err("ready server should reach the uninitialized test transport");
    assert!(format!("{ready_error:#}").contains("MCP client not initialized"));
}

#[tokio::test]
async fn connected_call_respects_server_tool_filters() {
    let client = create_ready_async_managed_client(vec![create_test_tool("docs", "search")]).await;
    client.client().await.expect("server should be ready");
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    manager.insert_test_client("docs", client);
    manager
        .servers
        .get_mut("docs")
        .expect("test server should exist")
        .tool_filter
        .disabled
        .insert("search".to_string());

    let filtered_error = manager
        .call_tool(
            "docs",
            "search",
            /*environment_id*/ None,
            /*arguments*/ None,
            /*meta*/ None,
            Some(Duration::from_secs(5)),
            /*wait_for_server*/ false,
        )
        .await
        .expect_err("disabled tools should not be callable");
    assert!(filtered_error.to_string().contains("disabled"));
}

#[tokio::test]
async fn call_tool_validates_environment_without_waiting_for_ready_connections() {
    let client = create_test_managed_client(vec![create_test_tool("docs", "search")]).await;
    let (client, _, _) = create_gated_async_managed_client(client);
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    manager.insert_test_client("docs", client);
    manager
        .servers
        .get_mut("docs")
        .expect("test server should exist")
        .metadata
        .environment_id = "executor-a".to_string();

    let mismatched_environment = manager
        .call_tool(
            "docs",
            "search",
            Some("executor-b"),
            /*arguments*/ None,
            /*meta*/ None,
            Some(Duration::from_secs(5)),
            /*wait_for_server*/ false,
        )
        .await
        .expect_err("calls must reject a server from a different environment");
    assert_eq!(
        mismatched_environment.to_string(),
        "MCP server `docs` is running in environment `executor-a`, expected `executor-b`"
    );

    let pending_call = tokio::time::timeout(
        Duration::from_millis(50),
        manager.call_tool(
            "docs",
            "search",
            Some("executor-a"),
            /*arguments*/ None,
            /*meta*/ None,
            Some(Duration::from_secs(5)),
            /*wait_for_server*/ false,
        ),
    )
    .await
    .expect("environment-scoped calls must not wait for pending server startup")
    .expect_err("pending server must not accept environment-scoped calls");
    assert!(pending_call.to_string().contains("not connected"));
}

#[tokio::test]
async fn list_all_tools_resolves_server_catalogs_concurrently() {
    let first_client = create_test_managed_client(vec![create_test_tool("first", "search")]).await;
    let second_client =
        create_test_managed_client(vec![create_test_tool("second", "lookup")]).await;
    let (first_client, first_started, release_first) =
        create_gated_async_managed_client(first_client);
    let (second_client, second_started, release_second) =
        create_gated_async_managed_client(second_client);
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    manager.insert_test_client("first", first_client);
    manager.insert_test_client("second", second_client);
    let manager = Arc::new(manager);
    let manager_for_listing = Arc::clone(&manager);
    let listing = tokio::spawn(async move { manager_for_listing.list_all_tools().await });

    tokio::time::timeout(Duration::from_secs(1), async {
        first_started.await.expect("first server startup");
        second_started.await.expect("second server startup");
    })
    .await
    .expect("both server catalogs should start before either is released");
    release_first.send(()).expect("release first server");
    release_second.send(()).expect("release second server");

    let tools = listing.await.expect("tool listing should complete");
    assert_eq!(
        model_tool_names(&tools),
        HashSet::from([
            ToolName::namespaced("mcp__first", "search"),
            ToolName::namespaced("mcp__second", "lookup"),
        ])
    );
}

#[tokio::test]
async fn list_all_tools_blocks_while_client_is_pending_without_cached_tools() {
    let pending_client = futures::future::pending::<Result<ManagedClient, StartupOutcomeError>>()
        .boxed()
        .shared();
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    manager.insert_test_client(
        CODEX_APPS_MCP_SERVER_NAME.to_string(),
        AsyncManagedClient {
            client: pending_client,

            server_capabilities: Arc::new(std::sync::Mutex::new(None)),

            tool_catalog_cache_context: None,
            startup_complete: Arc::new(std::sync::atomic::AtomicBool::new(false)),

            cancel_token: CancellationToken::new(),
        },
    );

    let timeout_result =
        tokio::time::timeout(Duration::from_millis(10), manager.list_all_tools()).await;
    assert!(timeout_result.is_err());
}

#[tokio::test]
async fn cancelling_startup_does_not_disable_a_ready_client() {
    let client = create_ready_async_managed_client(vec![create_test_tool("ready", "search")]).await;

    client.cancel_token.cancel();

    let managed = client
        .client()
        .await
        .expect("startup cancellation should not disable a ready client");
    assert_eq!(
        managed
            .tool_catalog
            .read(|catalog| model_tool_names(&catalog.tools))
            .await,
        HashSet::from([ToolName::namespaced("ready", "search")])
    );
}

#[tokio::test]
async fn shutdown_cancels_pending_tool_listing() {
    let cancel_token = CancellationToken::new();
    let cancel_token_for_startup = cancel_token.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let pending_client = async move {
        let _ = started_tx.send(());
        cancel_token_for_startup.cancelled().await;
        Err(StartupOutcomeError::Cancelled)
    }
    .boxed()
    .shared();
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    manager.insert_test_client(
        CODEX_APPS_MCP_SERVER_NAME.to_string(),
        AsyncManagedClient {
            client: pending_client,

            server_capabilities: Arc::new(std::sync::Mutex::new(None)),

            tool_catalog_cache_context: None,
            startup_complete: Arc::new(std::sync::atomic::AtomicBool::new(false)),

            cancel_token,
        },
    );
    let manager = Arc::new(manager);
    let manager_for_list = Arc::clone(&manager);
    let list_task = tokio::spawn(async move { manager_for_list.list_all_tools().await });

    started_rx.await.expect("tool listing should start");
    tokio::time::timeout(Duration::from_secs(1), manager.shutdown())
        .await
        .expect("shutdown should cancel speculative tool listing");
    let tools = list_task.await.expect("tool listing task should not panic");
    assert!(tools.is_empty());
}

#[tokio::test]
async fn shutdown_continues_after_caller_is_aborted() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let release_for_client = Arc::clone(&release);
    let blocking_client = async move {
        let _ = started_tx.send(());
        release_for_client.notified().await;
        let _ = completed_tx.send(());
        Err(StartupOutcomeError::Cancelled)
    }
    .boxed()
    .shared();
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    manager.insert_test_client(
        CODEX_APPS_MCP_SERVER_NAME.to_string(),
        AsyncManagedClient {
            client: blocking_client,

            server_capabilities: Arc::new(std::sync::Mutex::new(None)),

            tool_catalog_cache_context: None,
            startup_complete: Arc::new(std::sync::atomic::AtomicBool::new(false)),

            cancel_token: CancellationToken::new(),
        },
    );
    let manager = Arc::new(manager);
    let shutdown_task = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.shutdown().await }
    });

    started_rx.await.expect("client shutdown should start");
    shutdown_task.abort();
    let shutdown_error = shutdown_task
        .await
        .expect_err("caller shutdown task should be aborted");
    assert!(shutdown_error.is_cancelled());
    release.notify_one();

    tokio::time::timeout(Duration::from_secs(1), completed_rx)
        .await
        .expect("client shutdown should survive caller cancellation")
        .expect("client shutdown completion sender should stay alive");
}

#[tokio::test]
async fn list_all_tools_adds_server_metadata_to_tools() {
    let server_name = "docs";
    let managed_client =
        create_ready_async_managed_client(vec![create_test_tool(server_name, "search")]).await;
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    manager.insert_test_client(server_name, managed_client);
    manager.set_test_server_metadata(
        server_name,
        McpServerMetadata {
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            pollutes_memory: true,
            origin: Some(McpServerOrigin::StreamableHttp(
                "https://docs.example".to_string(),
            )),
            supports_parallel_tool_calls: true,
            default_tools_approval_mode: None,
            tool_approval_modes: HashMap::new(),
        },
    );

    let tools = manager.list_all_tools().await;
    assert_eq!(tools.len(), 1);
    let tool = &tools[0];
    assert_eq!(tool.server_name, server_name);
    assert!(tool.supports_parallel_tool_calls);
    assert_eq!(tool.server_origin.as_deref(), Some("https://docs.example"));
}

#[test]
fn server_metadata_preserves_tool_approval_policy() {
    let mut config = reusable_server_config("https://docs.example/mcp");
    config.environment_id = "remote".to_string();
    config.default_tools_approval_mode = Some(AppToolApproval::Prompt);
    config.tools.insert(
        "search".to_string(),
        McpServerToolConfig {
            approval_mode: Some(AppToolApproval::Approve),
            ..Default::default()
        },
    );
    let metadata = McpServerMetadata::from(&EffectiveMcpServer::from_host_config(config));

    assert_eq!(metadata.environment_id, "remote");
    assert_eq!(metadata.tool_approval_mode("read"), AppToolApproval::Prompt);
    assert_eq!(
        metadata.tool_approval_mode("search"),
        AppToolApproval::Approve
    );
}

#[tokio::test]
async fn no_local_runtime_fails_local_stdio_but_keeps_local_http_server() {
    let codex_home = tempdir().expect("tempdir");
    let mcp_servers = HashMap::from([
        (
            "stdio".to_string(),
            EffectiveMcpServer::from_host_config(McpServerConfig {
                auth: Default::default(),
                transport: McpServerTransportConfig::Stdio {
                    command: "echo".to_string(),
                    args: Vec::new(),
                    env: None,
                    env_vars: Vec::new(),
                    cwd: None,
                },
                environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
                enabled: true,
                required: false,
                startup_readiness: Default::default(),
                supports_parallel_tool_calls: false,
                tool_input_schema_max_bytes: None,
                omit_tools_from: None,
                disabled_reason: None,
                startup_timeout_sec: None,
                tool_timeout_sec: None,
                default_tools_approval_mode: None,
                enabled_tools: None,
                disabled_tools: None,
                scopes: None,
                oauth: None,
                oauth_resource: None,
                tools: HashMap::new(),
            }),
        ),
        (
            "http".to_string(),
            EffectiveMcpServer::from_host_config(McpServerConfig {
                auth: Default::default(),
                transport: McpServerTransportConfig::StreamableHttp {
                    url: "http://127.0.0.1:1".to_string(),
                    bearer_token_env_var: None,
                    http_headers: None,
                    env_http_headers: None,
                    http_headers_helper: None,
                },
                environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
                enabled: true,
                required: false,
                startup_readiness: Default::default(),
                supports_parallel_tool_calls: false,
                tool_input_schema_max_bytes: None,
                omit_tools_from: None,
                disabled_reason: None,
                startup_timeout_sec: None,
                tool_timeout_sec: None,
                default_tools_approval_mode: None,
                enabled_tools: None,
                disabled_tools: None,
                scopes: None,
                oauth: None,
                oauth_resource: None,
                tools: HashMap::new(),
            }),
        ),
    ]);

    let cancel_token = CancellationToken::new();
    let manager = McpConnectionSet::new(
        /*previous*/ None,
        McpPublicationGate::already_published(),
        McpRuntimeInput {
            startup_policy: McpStartupPolicy::Eager,
            config: Arc::new(crate::mcp::tests::test_mcp_config(
                codex_home.path().to_path_buf(),
            )),
            plugins_available: false,
            ready_selected_capability_roots: Vec::new(),
            mcp_servers,
            submit_id: String::new(),
            tx_event: None,
            startup_cancellation_token: cancel_token.clone(),
            runtime_context: McpRuntimeContext::new(
                Arc::new(environment_manager_without_environments()),
                PathBuf::from("/tmp"),
            ),
            codex_apps_tools_cache: ConnectorRuntimeManager::<ToolInfo>::default(),
            tool_catalog_cache: McpToolCatalogCache::default(),
            codex_apps_tools_cache_key: ConnectorRuntimeContextKey::personal(
                /*account_id*/ None, /*chatgpt_user_id*/ None,
            ),
            client_mcp_extensions: ClientMcpExtensions::default(),
            auth: None,
            auth_manager: None,
            elicitation_reviewer: None,
            elicitation_lifecycle: None,
        },
        ElicitationRequestRouter::default(),
    )
    .await;

    assert!(manager.contains_server("stdio"));
    assert!(manager.contains_server("http"));
    assert!(
        !manager
            .wait_for_server_ready("stdio", Duration::from_millis(10))
            .await
    );
    let error = match manager.test_client("stdio").client().await {
        Ok(_) => panic!("local stdio MCP startup should fail"),
        Err(error) => error,
    };
    let StartupOutcomeError::Failed { error, .. } = error else {
        panic!("local stdio MCP startup should fail rather than be cancelled");
    };
    assert_eq!(
        error,
        "local stdio MCP server `stdio` requires a local environment"
    );
    cancel_token.cancel();
}

#[test]
fn elicitation_capability_uses_2025_06_18_shape_for_form_only_support() {
    let capability = Some(ElicitationCapability::default());
    assert_eq!(
        serde_json::to_value(capability).expect("serialize elicitation capability"),
        serde_json::json!({})
    );
}

#[test]
fn elicitation_capability_advertises_url_support_when_enabled() {
    let capability = Some(
        ElicitationCapability::new()
            .with_form(rmcp::model::FormElicitationCapability::new())
            .with_url(rmcp::model::UrlElicitationCapability::new()),
    );
    assert_eq!(
        serde_json::to_value(capability).expect("serialize elicitation capability"),
        serde_json::json!({
            "form": {},
            "url": {},
        })
    );
}

#[test]
fn mcp_init_error_display_prompts_for_github_pat() {
    let server_name = "github";
    let config = McpServerConfig {
        auth: Default::default(),
        transport: McpServerTransportConfig::StreamableHttp {
            url: "https://api.githubcopilot.com/mcp/".to_string(),
            bearer_token_env_var: None,
            http_headers: None,
            env_http_headers: None,
            http_headers_helper: None,
        },
        environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
        enabled: true,
        required: false,
        startup_readiness: Default::default(),
        supports_parallel_tool_calls: false,
        tool_input_schema_max_bytes: None,
        omit_tools_from: None,
        disabled_reason: None,
        startup_timeout_sec: None,
        tool_timeout_sec: None,
        default_tools_approval_mode: None,
        enabled_tools: None,
        disabled_tools: None,
        scopes: None,
        oauth: None,
        oauth_resource: None,
        tools: HashMap::new(),
    };
    let err: StartupOutcomeError = anyhow::anyhow!("OAuth is unsupported").into();

    let display = mcp_init_error_display(server_name, Some(&config), &err, /*reason*/ None);

    let expected = format!(
        "GitHub MCP does not support OAuth. Log in by adding a personal access token (https://github.com/settings/personal-access-tokens) to your environment and config.toml:\n[mcp_servers.{server_name}]\nbearer_token_env_var = CODEX_GITHUB_PERSONAL_ACCESS_TOKEN"
    );

    assert_eq!(expected, display);
}

#[test]
fn mcp_init_error_display_prompts_for_login_when_auth_required() {
    let server_name = "example";
    let expected = format!(
        "The {server_name} MCP server is not logged in. Run `codex mcp login {server_name}`."
    );
    let executor_config: McpServerConfig = serde_json::from_value(serde_json::json!({
        "url": "https://example.com/mcp",
        "environment_id": "executor-1",
    }))
    .expect("executor MCP configuration should deserialize");

    for error in [
        anyhow::anyhow!("Auth required for server").into(),
        StartupOutcomeError::Failed {
            error: "OAuth refresh token was rejected: invalid_grant".to_string(),
            is_authentication_required: true,
        },
    ] {
        let display = mcp_init_error_display(
            server_name,
            /*config*/ None,
            &error,
            /*reason*/ None,
        );
        assert_eq!(expected, display);

        let executor_display = mcp_init_error_display(
            server_name,
            Some(&executor_config),
            &error,
            /*reason*/ None,
        );
        assert_eq!(
            format!(
                "The {server_name} MCP server is not logged in. Use your client's MCP OAuth sign-in flow."
            ),
            executor_display
        );
    }
}

#[test]
fn mcp_init_error_display_identifies_oauth_reauthentication() {
    let server_name = "example";
    let error = StartupOutcomeError::Failed {
        error: "authorization required: Bearer error=\"invalid_token\"".to_string(),
        is_authentication_required: true,
    };
    let executor_config: McpServerConfig = serde_json::from_value(serde_json::json!({
        "url": "https://example.com/mcp",
        "environment_id": "executor-1",
    }))
    .expect("executor MCP configuration should deserialize");

    for (config, recovery_hint) in [
        (None, "Run `codex mcp login example`."),
        (
            Some(&executor_config),
            "Use your client's MCP OAuth sign-in flow.",
        ),
    ] {
        assert_eq!(
            mcp_init_error_display(
                server_name,
                config,
                &error,
                Some(McpStartupFailureReason::ReauthenticationRequired),
            ),
            format!(
                "The {server_name} MCP server requires OAuth reauthentication. {recovery_hint}"
            ),
        );
    }
}

#[test]
fn mcp_startup_failure_reason_requires_existing_oauth_and_auth_failure() {
    for (auth_state, is_authentication_required, expected) in [
        (
            Some(McpAuthState::LoggedOut(
                McpLoginRequirement::Reauthentication,
            )),
            true,
            Some(McpStartupFailureReason::ReauthenticationRequired),
        ),
        (
            Some(McpAuthState::LoggedOut(
                McpLoginRequirement::Reauthentication,
            )),
            false,
            None,
        ),
        (
            Some(McpAuthState::LoggedOut(McpLoginRequirement::Login)),
            true,
            None,
        ),
        (Some(McpAuthState::Unsupported), true, None),
        (Some(McpAuthState::BearerToken), true, None),
        (
            Some(McpAuthState::OAuth),
            true,
            Some(McpStartupFailureReason::ReauthenticationRequired),
        ),
        (Some(McpAuthState::OAuth), false, None),
        (None, true, None),
    ] {
        let error = StartupOutcomeError::Failed {
            error: "startup failed".to_string(),
            is_authentication_required,
        };

        assert_eq!(
            mcp_startup_failure_reason(auth_state, &error),
            expected,
            "auth_state={auth_state:?}, is_authentication_required={is_authentication_required}"
        );
    }
}

#[test]
fn mcp_init_error_display_reports_generic_errors() {
    let server_name = "custom";
    let config = McpServerConfig {
        auth: Default::default(),
        transport: McpServerTransportConfig::StreamableHttp {
            url: "https://example.com".to_string(),
            bearer_token_env_var: Some("TOKEN".to_string()),
            http_headers: None,
            env_http_headers: None,
            http_headers_helper: None,
        },
        environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
        enabled: true,
        required: false,
        startup_readiness: Default::default(),
        supports_parallel_tool_calls: false,
        tool_input_schema_max_bytes: None,
        omit_tools_from: None,
        disabled_reason: None,
        startup_timeout_sec: None,
        tool_timeout_sec: None,
        default_tools_approval_mode: None,
        enabled_tools: None,
        disabled_tools: None,
        scopes: None,
        oauth: None,
        oauth_resource: None,
        tools: HashMap::new(),
    };
    let err: StartupOutcomeError = anyhow::anyhow!("boom").into();

    let display = mcp_init_error_display(server_name, Some(&config), &err, /*reason*/ None);

    let expected = format!("MCP client for `{server_name}` failed to start: {err:#}");

    assert_eq!(expected, display);
}

#[test]
fn mcp_init_error_display_quotes_server_names() {
    let github_config: McpServerConfig = serde_json::from_value(serde_json::json!({
        "url": "https://api.githubcopilot.com/mcp/",
    }))
    .expect("GitHub MCP configuration should deserialize");
    let error: StartupOutcomeError = anyhow::anyhow!("request timed out").into();
    let mut displays = Vec::new();
    for server_name in ["npm:@scope/package.name", "server.name"] {
        for config in [None, Some(&github_config)] {
            displays.push(mcp_init_error_display(
                server_name,
                config,
                &error,
                /*reason*/ None,
            ));
        }
    }
    insta::assert_snapshot!(displays.join("\n\n"));
}

#[test]
fn mcp_init_error_display_includes_startup_timeout_hint() {
    let server_name = "slow";
    for error in [
        "request timed out",
        "MCP client startup timed out after 30s",
    ] {
        let err: StartupOutcomeError = anyhow::anyhow!(error).into();

        let display = mcp_init_error_display(
            server_name,
            /*config*/ None,
            &err,
            /*reason*/ None,
        );

        assert_eq!(
            "MCP client for `slow` timed out after 30 seconds. Add or adjust `startup_timeout_sec` in your config.toml:\n[mcp_servers.slow]\nstartup_timeout_sec = XX",
            display
        );
    }
}

fn reusable_server_config(url: &str) -> McpServerConfig {
    McpServerConfig {
        auth: Default::default(),
        transport: McpServerTransportConfig::StreamableHttp {
            url: url.to_string(),
            bearer_token_env_var: Some("CODEX_MCP_REUSE_TEST_TOKEN".to_string()),
            http_headers: None,
            env_http_headers: None,
            http_headers_helper: None,
        },
        environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
        enabled: true,
        required: false,
        startup_readiness: Default::default(),
        supports_parallel_tool_calls: false,
        tool_input_schema_max_bytes: None,
        omit_tools_from: None,
        disabled_reason: None,
        startup_timeout_sec: None,
        tool_timeout_sec: None,
        default_tools_approval_mode: None,
        enabled_tools: None,
        disabled_tools: None,
        scopes: None,
        oauth: None,
        oauth_resource: None,
        tools: HashMap::new(),
    }
}

fn reusable_server_runtime_context() -> McpRuntimeContext {
    McpRuntimeContext::new(
        Arc::new(environment_manager_without_environments()),
        PathBuf::from("/tmp"),
    )
}

fn reusable_server_identity(
    server_name: &str,
    config: &McpServerConfig,
    runtime_context: &McpRuntimeContext,
) -> McpServerConnectionIdentity {
    let server = EffectiveMcpServer::from_host_config(config.clone());
    let resolved_environment = runtime_context.resolve_server_environment(server_name, config);
    McpServerConnectionIdentity::new(
        server_name,
        &server,
        /*host_plugin_root*/ None,
        OAuthCredentialsStoreMode::default(),
        AuthKeyringBackendKind::default(),
        McpOAuthRefreshMode::Legacy,
        &resolved_environment,
        runtime_context,
        ElicitationCapability::default(),
        ClientMcpExtensions::default(),
        /*previous_identity*/ None,
    )
}

async fn manager_with_reusable_ready_server(
    config: &McpServerConfig,
    runtime_context: &McpRuntimeContext,
    tools: Vec<ToolInfo>,
) -> McpConnectionSet {
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut manager = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    let server = EffectiveMcpServer::from_host_config(config.clone());
    manager.servers.insert(
        "docs".to_string(),
        McpServerView {
            protocol_mode: crate::McpProtocolMode::Legacy,
            startup_readiness: Default::default(),
            connection: Arc::new(McpServerConnection {
                identity: Some(reusable_server_identity("docs", config, runtime_context)),
                client: create_ready_async_managed_client(tools).await,
                startup_timeout: config
                    .startup_timeout_sec
                    .unwrap_or(DEFAULT_STARTUP_TIMEOUT),
                startup_trigger: None,
                _diagnostics_guard: LIVE_CONNECTIONS.track(),
            }),
            metadata: McpServerMetadata::from(&server),
            tool_filter: ToolFilter::from_config(config),
            tool_timeout: Some(config.tool_timeout_sec.unwrap_or(DEFAULT_TOOL_TIMEOUT)),
            catalog_item_limit: crate::pagination::MAX_MCP_CATALOG_ITEMS,
        },
    );
    manager
}

async fn reconcile_reusable_server(
    previous: &McpConnectionSet,
    config: McpServerConfig,
    runtime_context: McpRuntimeContext,
) -> McpConnectionSet {
    let codex_home = tempdir().expect("tempdir");
    reconcile_reusable_server_with_mcp_config(
        previous,
        "docs",
        config,
        runtime_context,
        crate::mcp::tests::test_mcp_config(codex_home.path().to_path_buf()),
    )
    .await
}

async fn reconcile_reusable_server_with_mcp_config(
    previous: &McpConnectionSet,
    server_name: &str,
    config: McpServerConfig,
    runtime_context: McpRuntimeContext,
    mcp_config: crate::McpConfig,
) -> McpConnectionSet {
    let (tx_event, _rx_event) = async_channel::unbounded();
    McpConnectionSet::new(
        Some(previous),
        McpPublicationGate::already_published(),
        McpRuntimeInput {
            startup_policy: McpStartupPolicy::Eager,
            config: Arc::new(mcp_config),
            plugins_available: false,
            ready_selected_capability_roots: Vec::new(),
            mcp_servers: HashMap::from([(
                server_name.to_string(),
                EffectiveMcpServer::from_host_config(config),
            )]),
            submit_id: "refresh".to_string(),
            tx_event: Some(tx_event),
            startup_cancellation_token: CancellationToken::new(),
            runtime_context,
            codex_apps_tools_cache: ConnectorRuntimeManager::default(),
            tool_catalog_cache: McpToolCatalogCache::default(),
            codex_apps_tools_cache_key: ConnectorRuntimeContextKey::personal(
                /*account_id*/ None, /*chatgpt_user_id*/ None,
            ),
            client_mcp_extensions: ClientMcpExtensions::default(),
            auth: None,
            auth_manager: None,
            elicitation_reviewer: None,
            elicitation_lifecycle: None,
        },
        ElicitationRequestRouter::default(),
    )
    .await
}

#[tokio::test]
async fn read_only_policy_does_not_reuse_a_writable_connection() {
    let codex_home = tempdir().expect("tempdir");
    let runtime_context = reusable_server_runtime_context();
    let config = reusable_server_config("http://127.0.0.1:1");
    let previous = manager_with_reusable_ready_server(
        &config,
        &runtime_context,
        vec![create_test_tool("docs", "write")],
    )
    .await;
    for read_only in [false, true] {
        let mut mcp_config = crate::mcp::tests::test_mcp_config(codex_home.path().to_path_buf());
        mcp_config.requires_read_only_mcp_tools = read_only;
        let reconciled = reconcile_reusable_server_with_mcp_config(
            &previous,
            "docs",
            config.clone(),
            runtime_context.clone(),
            mcp_config,
        )
        .await;
        assert_eq!(
            previous.shares_test_connection_with(&reconciled, "docs"),
            !read_only
        );
    }
}

#[tokio::test]
async fn reconciliation_reuses_connection_without_relisting_regular_tools() -> anyhow::Result<()> {
    let tools = Arc::new(tokio::sync::RwLock::new(vec![Tool::new(
        "old_search",
        "old search",
        Arc::new(JsonObject::default()),
    )]));
    let block_tool_listing = Arc::new(AtomicBool::new(false));
    let client = Arc::new(
        RmcpClient::new_in_process_client(Arc::new(MutableToolsTransportFactory {
            server: MutableToolsServer {
                tools: Arc::clone(&tools),
                block_tool_listing: Arc::clone(&block_tool_listing),
            },
        }))
        .await?,
    );
    let initialize = client
        .initialize(
            InitializeRequestParams::new(
                ClientCapabilities::default(),
                Implementation::new("codex-test", "0.0.0-test"),
            )
            .with_protocol_version(ProtocolVersion::V_2025_06_18),
            /*timeout*/ None,
            Box::new(|_, _| {
                async {
                    Ok(ElicitationResponse {
                        action: ElicitationAction::Decline,
                        content: None,
                        meta: None,
                    })
                }
                .boxed()
            }),
        )
        .await?;
    let initial_tools = list_tools_for_client_uncached(
        "docs",
        &client,
        /*timeout*/ None,
        crate::pagination::MAX_MCP_CATALOG_ITEMS,
        initialize.instructions.as_deref(),
    )
    .await?;
    let managed_client = ManagedClient {
        _auth_change_notifications: None,
        client,
        server_info: create_test_server_info("Mutable tools"),
        tool_catalog: Arc::new(ClientToolCatalog::new(initial_tools, /*updates*/ None)),
        tool_timeout: None,
        server_instructions: initialize.instructions,
        server_supports_sandbox_state_meta_capability: false,
    };
    let runtime_context = reusable_server_runtime_context();
    let config = reusable_server_config("http://127.0.0.1:1");
    let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let permission_profile = Constrained::allow_any(PermissionProfile::default());
    let mut previous = McpConnectionSet::new_uninitialized(
        &approval_policy,
        &permission_profile,
        /*prefix_mcp_tool_names*/ true,
    );
    let server = EffectiveMcpServer::from_host_config(config.clone());
    previous.servers.insert(
        "docs".to_string(),
        McpServerView {
            protocol_mode: crate::McpProtocolMode::Legacy,
            startup_readiness: Default::default(),
            connection: Arc::new(McpServerConnection {
                identity: Some(reusable_server_identity("docs", &config, &runtime_context)),
                client: AsyncManagedClient {
                    client: futures::future::ready(Ok(managed_client)).boxed().shared(),

                    server_capabilities: Arc::new(std::sync::Mutex::new(None)),

                    tool_catalog_cache_context: None,
                    startup_complete: Arc::new(std::sync::atomic::AtomicBool::new(true)),

                    cancel_token: CancellationToken::new(),
                },
                startup_timeout: config
                    .startup_timeout_sec
                    .unwrap_or(DEFAULT_STARTUP_TIMEOUT),
                startup_trigger: None,
                _diagnostics_guard: LIVE_CONNECTIONS.track(),
            }),
            metadata: McpServerMetadata::from(&server),
            tool_filter: ToolFilter::from_config(&config),
            tool_timeout: Some(config.tool_timeout_sec.unwrap_or(DEFAULT_TOOL_TIMEOUT)),
            catalog_item_limit: crate::pagination::MAX_MCP_CATALOG_ITEMS,
        },
    );
    let previous = Arc::new(previous);
    let old_step = capture_binding(&previous).await;
    *tools.write().await = vec![Tool::new(
        "new_search",
        "new search",
        Arc::new(JsonObject::default()),
    )];
    block_tool_listing.store(true, Ordering::Release);

    let reconciled = Arc::new(
        tokio::time::timeout(
            Duration::from_secs(1),
            reconcile_reusable_server(&previous, config, runtime_context),
        )
        .await
        .expect("connection reuse must not wait for a tool-list request"),
    );
    let new_step = capture_binding(&reconciled).await;

    assert!(previous.shares_test_connection_with(&reconciled, "docs"));
    assert_eq!(
        old_step
            .tools()
            .iter()
            .map(|tool| tool.tool.name.to_string())
            .collect::<Vec<_>>(),
        vec!["old_search".to_string()]
    );
    assert_eq!(
        new_step
            .tools()
            .iter()
            .map(|tool| tool.tool.name.to_string())
            .collect::<Vec<_>>(),
        vec!["old_search".to_string()]
    );
    Ok(())
}

#[tokio::test]
async fn reconciliation_reuses_an_unchanged_ready_server() {
    let runtime_context = reusable_server_runtime_context();
    let config = reusable_server_config("http://127.0.0.1:1");
    let previous = manager_with_reusable_ready_server(
        &config,
        &runtime_context,
        vec![create_test_tool("docs", "search")],
    )
    .await;

    let reconciled = reconcile_reusable_server(&previous, config, runtime_context.clone()).await;

    assert!(previous.shares_test_connection_with(&reconciled, "docs"));
    assert_eq!(
        model_tool_names(&reconciled.list_all_tools().await),
        HashSet::from([ToolName::namespaced("mcp__docs", "search")])
    );
}

#[tokio::test]
async fn reconciliation_reuses_an_unchanged_pending_server_without_waiting() -> anyhow::Result<()> {
    let runtime_context = reusable_server_runtime_context();
    let mut config = reusable_server_config("http://127.0.0.1:1");
    let tools = vec![
        create_test_tool("docs", "search"),
        create_test_tool("docs", "write"),
    ];
    let mut previous =
        manager_with_reusable_ready_server(&config, &runtime_context, tools.clone()).await;
    let managed_client = create_test_managed_client(tools).await;
    let (pending_client, startup_started, release_startup) =
        create_gated_async_managed_client(managed_client);
    let startup = tokio::spawn({
        let pending_client = pending_client.clone();
        async move { pending_client.client().await }
    });
    startup_started.await?;
    let connection = Arc::get_mut(
        &mut previous
            .servers
            .get_mut("docs")
            .expect("test server should exist")
            .connection,
    )
    .expect("test server should have one connection owner");
    connection.client = pending_client;
    config.enabled_tools = Some(vec!["search".to_string()]);
    config.startup_timeout_sec = Some(DEFAULT_STARTUP_TIMEOUT);

    let reconciled = tokio::time::timeout(
        Duration::from_millis(100),
        reconcile_reusable_server(&previous, config, runtime_context),
    )
    .await
    .expect("reconciliation must not wait for an unchanged pending MCP server");

    assert!(previous.shares_test_connection_with(&reconciled, "docs"));
    release_startup
        .send(())
        .map_err(|()| anyhow!("pending startup should still be running"))?;
    startup.await??;
    assert_eq!(
        model_tool_names(&reconciled.list_all_tools().await),
        HashSet::from([ToolName::namespaced("mcp__docs", "search")])
    );
    Ok(())
}

#[tokio::test]
async fn reconciliation_cancels_a_reused_pending_server_when_disabled() -> anyhow::Result<()> {
    let runtime_context = reusable_server_runtime_context();
    let mut config = reusable_server_config("http://127.0.0.1:1");
    let tools = vec![create_test_tool("docs", "search")];
    let mut previous =
        manager_with_reusable_ready_server(&config, &runtime_context, tools.clone()).await;
    let managed_client = create_test_managed_client(tools).await;
    let (pending_client, startup_started, release_startup) =
        create_gated_async_managed_client(managed_client);
    let cancellation = pending_client.cancel_token.clone();
    let startup = tokio::spawn({
        let pending_client = pending_client.clone();
        async move { pending_client.client().await }
    });
    startup_started.await?;
    let connection = Arc::get_mut(
        &mut previous
            .servers
            .get_mut("docs")
            .expect("test server should exist")
            .connection,
    )
    .expect("test server should have one connection owner");
    connection.client = pending_client;

    let reused =
        reconcile_reusable_server(&previous, config.clone(), runtime_context.clone()).await;
    assert!(previous.shares_test_connection_with(&reused, "docs"));

    config.enabled = false;
    let removed = reconcile_reusable_server(&reused, config, runtime_context).await;
    assert!(!removed.servers.contains_key("docs"));
    drop(previous);
    drop(reused);

    assert!(
        cancellation.is_cancelled(),
        "disabling a reused pending MCP server should cancel its obsolete startup"
    );
    release_startup
        .send(())
        .map_err(|()| anyhow!("pending startup should remain available for test cleanup"))?;
    startup.await??;
    Ok(())
}

#[tokio::test]
async fn reconciliation_retries_non_oauth_authentication_failures() {
    let runtime_context = reusable_server_runtime_context();
    let config = reusable_server_config("http://127.0.0.1:1");
    let mut previous =
        manager_with_reusable_ready_server(&config, &runtime_context, Vec::new()).await;
    let connection =
        Arc::get_mut(&mut previous.servers.get_mut("docs").expect("server").connection)
            .expect("test server has one connection owner");
    connection.client.client = futures::future::ready(Err(StartupOutcomeError::Failed {
        error: "bearer token rejected".to_string(),
        is_authentication_required: true,
    }))
    .boxed()
    .shared();

    let reconciled = reconcile_reusable_server(&previous, config, runtime_context).await;

    assert!(!previous.shares_test_connection_with(&reconciled, "docs"));
}

#[test]
fn connection_identity_tracks_only_oauth_credentials_when_oauth_is_active() {
    let runtime_context = reusable_server_runtime_context();
    for authorization in [None, Some("Bearer configured-token")] {
        let identity = |oauth: serde_json::Value| {
            let config: McpServerConfig = serde_json::from_value(serde_json::json!({
                "url": "http://127.0.0.1:1",
                "http_headers": authorization.map(|value| HashMap::from([
                    ("Authorization", value)
                ])),
                "oauth": oauth
            }))
            .expect("valid OAuth configuration");
            let debug = format!("{config:?}");
            assert!(!debug.contains("old-secret"));
            assert!(!debug.contains("new-secret"));
            McpServerConnectionIdentity::new(
                "docs",
                &EffectiveMcpServer::from_host_config(config),
                /*host_plugin_root*/ None,
                OAuthCredentialsStoreMode::File,
                AuthKeyringBackendKind::Direct,
                McpOAuthRefreshMode::Legacy,
                &Ok(None),
                &runtime_context,
                ElicitationCapability::default(),
                ClientMcpExtensions::default(),
                /*previous_identity*/ None,
            )
        };
        let original_config = serde_json::json!({
            "client_id": "client",
            "client_secret": "old-secret",
            "callback_url": "http://127.0.0.1:12799/callback/original",
            "callback_port": 12799,
        });
        let original = identity(original_config.clone());
        assert!(original.has_same_connection_config(&identity(original_config.clone())));
        for (field, value) in [
            ("client_secret", serde_json::json!("new-secret")),
            ("client_id", serde_json::json!("new-client")),
        ] {
            let mut changed = original_config.clone();
            changed[field] = value;
            assert_eq!(
                original.has_same_connection_config(&identity(changed)),
                authorization.is_some(),
            );
        }
        for (field, value) in [
            (
                "callback_url",
                serde_json::json!("http://127.0.0.1:12799/callback/changed"),
            ),
            ("callback_port", serde_json::json!(12800)),
        ] {
            let mut changed = original_config.clone();
            changed[field] = value.clone();
            assert!(original.has_same_connection_config(&identity(changed)));

            let mut callback_only = serde_json::json!({});
            callback_only[field] = value;
            assert!(
                identity(serde_json::Value::Null)
                    .has_same_connection_config(&identity(callback_only))
            );
        }
    }
}

#[test]
fn connection_identity_uses_effective_authorization_headers() {
    let runtime_context = reusable_server_runtime_context();
    let missing_env_var = format!("CODEX_TEST_UNSET_MCP_AUTHORIZATION_{}", std::process::id());
    assert!(std::env::var_os(&missing_env_var).is_none());

    for (static_header, environment_header, has_authorization) in [
        (Some("Bearer configured-token"), None, true),
        (Some("invalid\nheader"), None, false),
        (None, Some("PATH"), true),
        (None, Some(missing_env_var.as_str()), false),
        (None, None, false),
    ] {
        let mut config = reusable_server_config("http://127.0.0.1:1");
        config.transport = McpServerTransportConfig::StreamableHttp {
            url: "http://127.0.0.1:1".to_string(),
            bearer_token_env_var: None,
            http_headers: static_header
                .map(|value| HashMap::from([("aUtHoRiZaTiOn".to_string(), value.to_string())])),
            env_http_headers: environment_header
                .map(|value| HashMap::from([("aUtHoRiZaTiOn".to_string(), value.to_string())])),
            http_headers_helper: None,
        };
        let server = EffectiveMcpServer::from_host_config(config);
        let identity = |keyring_backend_kind, oauth_refresh_mode| {
            McpServerConnectionIdentity::new(
                "docs",
                &server,
                /*host_plugin_root*/ None,
                OAuthCredentialsStoreMode::File,
                keyring_backend_kind,
                oauth_refresh_mode,
                &Ok(None),
                &runtime_context,
                ElicitationCapability::default(),
                ClientMcpExtensions::default(),
                /*previous_identity*/ None,
            )
        };

        assert_eq!(
            identity(AuthKeyringBackendKind::Direct, McpOAuthRefreshMode::Legacy)
                .has_same_connection_config(&identity(
                    AuthKeyringBackendKind::Secrets,
                    McpOAuthRefreshMode::Legacy,
                )),
            has_authorization,
        );
        assert_eq!(
            identity(AuthKeyringBackendKind::Direct, McpOAuthRefreshMode::Legacy)
                .has_same_connection_config(&identity(
                    AuthKeyringBackendKind::Direct,
                    McpOAuthRefreshMode::Coordinated,
                )),
            has_authorization,
        );
    }
}

#[tokio::test]
async fn reconciliation_reuses_legacy_stdio_server_with_existing_protocol_marker() {
    let runtime_context = McpRuntimeContext::new(
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        PathBuf::from("/tmp"),
    );
    let mut config = reusable_server_config("http://127.0.0.1:1");
    config.transport = McpServerTransportConfig::Stdio {
        command: "legacy-server".to_string(),
        args: Vec::new(),
        env: Some(HashMap::from([(
            "CODEX_MCP_PROTOCOL_VERSION".to_string(),
            "1999-01-01".to_string(),
        )])),
        env_vars: Vec::new(),
        cwd: None,
    };
    let previous = manager_with_reusable_ready_server(
        &config,
        &runtime_context,
        vec![create_test_tool("docs", "search")],
    )
    .await;

    let reconciled = reconcile_reusable_server(&previous, config, runtime_context).await;

    assert!(previous.shares_test_connection_with(&reconciled, "docs"));
}

#[tokio::test]
async fn reconciliation_replaces_connection_when_auth_mode_changes() -> anyhow::Result<()> {
    let environment_manager = Arc::new(environment_manager_without_environments());
    environment_manager.upsert_environment(
        "customer-executor".to_string(),
        "ws://127.0.0.1:1".to_string(),
        /*connect_timeout*/ None,
    )?;
    let runtime_context = McpRuntimeContext::new(environment_manager, PathBuf::from("/tmp"));
    let codex_home = tempdir()?;
    let mcp_config = crate::mcp::tests::test_mcp_config(codex_home.path().to_path_buf());
    let [config, refreshed_config] = [McpServerAuth::OAuth, McpServerAuth::ChatGpt].map(|auth| {
        let mut config = reusable_server_config("https://chatgpt.com/backend-api/ps/mcp");
        config.environment_id = "customer-executor".to_string();
        config.auth = auth;
        let mut mcp_config = mcp_config.clone();
        let mut catalog = crate::ResolvedMcpCatalog::builder();
        catalog.register(crate::McpServerRegistration::from_config(
            "docs".to_string(),
            config.clone(),
        ));
        mcp_config.mcp_server_catalog = catalog.build();
        crate::effective_mcp_servers_from_configured(
            HashMap::from([("docs".to_string(), config)]),
            &mcp_config,
            /*auth*/ None,
        )
        .remove("docs")
        .expect("configured server should survive auth projection")
        .config()
        .clone()
    });
    let previous = manager_with_reusable_ready_server(
        &config,
        &runtime_context,
        vec![create_test_tool("docs", "search")],
    )
    .await;

    let reconciled = reconcile_reusable_server(&previous, refreshed_config, runtime_context).await;
    let outcome = reconciled
        .servers
        .get("docs")
        .expect("refreshed server should exist")
        .connection
        .client()
        .await;
    assert_matches!(
        outcome.err().expect("changed auth mode must be validated"),
        StartupOutcomeError::Failed {
            error,
            is_authentication_required,
        } => {
            assert_eq!(
                (error.as_str(), is_authentication_required),
                (
                    "ChatGPT MCP authentication is unsupported; configure the MCP server's own credentials or OAuth",
                    false,
                )
            );
        }
    );
    assert_eq!(
        model_tool_names(&reconciled.list_all_tools().await),
        HashSet::new()
    );
    Ok(())
}

#[tokio::test]
async fn reconciliation_replaces_connection_when_protocol_mode_changes() {
    let runtime_context = reusable_server_runtime_context();
    let config = reusable_server_config("http://127.0.0.1:1");
    let previous = manager_with_reusable_ready_server(
        &config,
        &runtime_context,
        vec![create_test_tool("docs", "search")],
    )
    .await;
    let codex_home = tempdir().expect("tempdir");
    let mut mcp_config = crate::mcp::tests::test_mcp_config(codex_home.path().to_path_buf());
    mcp_config.protocol_mode = codex_rmcp_client::McpProtocolMode::V20260728;

    let reconciled = McpConnectionSet::new(
        Some(&previous),
        McpPublicationGate::already_published(),
        McpRuntimeInput {
            startup_policy: McpStartupPolicy::Eager,
            config: Arc::new(mcp_config),
            plugins_available: false,
            ready_selected_capability_roots: Vec::new(),
            mcp_servers: HashMap::from([(
                "docs".to_string(),
                EffectiveMcpServer::from_host_config(config),
            )]),
            submit_id: "refresh".to_string(),
            tx_event: None,
            startup_cancellation_token: CancellationToken::new(),
            runtime_context,
            codex_apps_tools_cache: ConnectorRuntimeManager::default(),
            tool_catalog_cache: McpToolCatalogCache::default(),
            codex_apps_tools_cache_key: ConnectorRuntimeContextKey::personal(
                /*account_id*/ None, /*chatgpt_user_id*/ None,
            ),
            client_mcp_extensions: ClientMcpExtensions::default(),
            auth: None,
            auth_manager: None,
            elicitation_reviewer: None,
            elicitation_lifecycle: None,
        },
        ElicitationRequestRouter::default(),
    )
    .await;

    assert!(!previous.shares_test_connection_with(&reconciled, "docs"));
}

#[tokio::test]
async fn reconciliation_reuses_legacy_stdio_server_when_modern_protocol_is_enabled() {
    let runtime_context = McpRuntimeContext::new(
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        PathBuf::from("/tmp"),
    );
    let mut config = reusable_server_config("http://127.0.0.1:1");
    config.transport = McpServerTransportConfig::Stdio {
        command: "legacy-server".to_string(),
        args: Vec::new(),
        env: None,
        env_vars: Vec::new(),
        cwd: None,
    };
    let previous = manager_with_reusable_ready_server(
        &config,
        &runtime_context,
        vec![create_test_tool("docs", "search")],
    )
    .await;
    let codex_home = tempdir().expect("tempdir");
    let mut mcp_config = crate::mcp::tests::test_mcp_config(codex_home.path().to_path_buf());
    mcp_config.protocol_mode = codex_rmcp_client::McpProtocolMode::V20260728;

    let reconciled = McpConnectionSet::new(
        Some(&previous),
        McpPublicationGate::already_published(),
        McpRuntimeInput {
            startup_policy: McpStartupPolicy::Eager,
            config: Arc::new(mcp_config),
            plugins_available: false,
            ready_selected_capability_roots: Vec::new(),
            mcp_servers: HashMap::from([(
                "docs".to_string(),
                EffectiveMcpServer::from_host_config(config),
            )]),
            submit_id: "refresh".to_string(),
            tx_event: None,
            startup_cancellation_token: CancellationToken::new(),
            runtime_context,
            codex_apps_tools_cache: ConnectorRuntimeManager::default(),
            tool_catalog_cache: McpToolCatalogCache::default(),
            codex_apps_tools_cache_key: ConnectorRuntimeContextKey::personal(
                /*account_id*/ None, /*chatgpt_user_id*/ None,
            ),
            client_mcp_extensions: ClientMcpExtensions::default(),
            auth: None,
            auth_manager: None,
            elicitation_reviewer: None,
            elicitation_lifecycle: None,
        },
        ElicitationRequestRouter::default(),
    )
    .await;

    assert!(previous.shares_test_connection_with(&reconciled, "docs"));
}

#[tokio::test]
async fn reconciliation_updates_elicitation_policy_without_restarting_ready_server() {
    let runtime_context = reusable_server_runtime_context();
    let config = reusable_server_config("http://127.0.0.1:1");
    let mut previous = manager_with_reusable_ready_server(
        &config,
        &runtime_context,
        vec![create_test_tool("docs", "search")],
    )
    .await;
    let router = ElicitationRequestRouter::default();
    previous.elicitation_requests = ElicitationRequestManager::new(
        test_elicitation_config("docs", AskForApproval::Never, PermissionProfile::Disabled),
        /*reviewer*/ None,
        /*lifecycle*/ None,
        router.clone(),
    );
    let (tx_event, events) = async_channel::unbounded();
    let sender = previous.elicitation_requests.make_sender(
        "docs".to_string(),
        Some(tx_event),
        &ClientMcpExtensions::default(),
    );
    let elicitation =
        codex_rmcp_client::Elicitation::Mcp(ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: "What should I say?".to_string(),
            requested_schema: requested_user_input_schema(),
        });
    let response = ElicitationResponse {
        action: ElicitationAction::Accept,
        content: Some(serde_json::json!({"message": "continue"})),
        meta: None,
    };

    for approval_policy in [
        AskForApproval::OnRequest,
        AskForApproval::Never,
        AskForApproval::OnRequest,
    ] {
        let mcp_config =
            test_elicitation_config("docs", approval_policy, PermissionProfile::default());
        let reconciled = reconcile_reusable_server_with_mcp_config(
            &previous,
            "docs",
            config.clone(),
            runtime_context.clone(),
            mcp_config.as_ref().clone(),
        )
        .await;
        assert!(previous.shares_test_connection_with(&reconciled, "docs"));
        {
            let authority = reconciled
                .elicitation_requests
                .authority
                .lock()
                .expect("elicitation authority lock");
            let config = &authority.as_ref().expect("elicitation authority").config;
            assert_eq!(config.approval_policy.value(), approval_policy);
            assert_eq!(config.permission_profile, PermissionProfile::default());
        }

        // A sender captured before reconciliation must observe each policy update.
        let mut pending = sender(NumberOrString::Number(7), elicitation.clone());
        if approval_policy == AskForApproval::OnRequest {
            assert!(futures::poll!(pending.as_mut()).is_pending());
            let EventMsg::ElicitationRequest(request) =
                events.try_recv().expect("user-input event").msg
            else {
                panic!("expected MCP elicitation");
            };
            let codex_protocol::mcp::RequestId::String(request_id) = request.id else {
                panic!("expected Codex-owned string request ID");
            };
            router
                .resolve(
                    "docs".to_string(),
                    NumberOrString::String(request_id.into()),
                    response.clone(),
                )
                .await
                .expect("user response should resolve the retained sender");
            assert_eq!(pending.await.expect("elicitation should resolve"), response);
        } else {
            assert_eq!(
                pending
                    .now_or_never()
                    .expect("a policy denial must not wait for user input")
                    .expect("elicitation should receive a response"),
                ElicitationResponse {
                    action: ElicitationAction::Decline,
                    content: None,
                    meta: None,
                }
            );
        }
        assert!(events.is_empty());
        previous = reconciled;
    }
}

#[tokio::test]
async fn reconciliation_reuses_ready_server_when_startup_timeout_changes() {
    let runtime_context = reusable_server_runtime_context();
    let mut config = reusable_server_config("http://127.0.0.1:1");
    let previous = manager_with_reusable_ready_server(
        &config,
        &runtime_context,
        vec![create_test_tool("docs", "search")],
    )
    .await;
    config.startup_timeout_sec = Some(Duration::from_secs(60));

    let reconciled = reconcile_reusable_server(&previous, config, runtime_context).await;

    assert_eq!(
        model_tool_names(&reconciled.list_all_tools().await),
        HashSet::from([ToolName::namespaced("mcp__docs", "search")])
    );
}

#[tokio::test]
async fn reconciliation_replaces_closed_connections() -> anyhow::Result<()> {
    let runtime_context = reusable_server_runtime_context();
    let config = reusable_server_config("http://127.0.0.1:1");
    let mut previous = manager_with_reusable_ready_server(
        &config,
        &runtime_context,
        vec![create_test_tool("docs", "search")],
    )
    .await;
    let disconnect = CancellationToken::new();
    let client = Arc::new(
        RmcpClient::new_in_process_client(Arc::new(DisconnectingToolsTransportFactory {
            server: MutableToolsServer {
                tools: Arc::new(tokio::sync::RwLock::new(vec![Tool::new(
                    "search",
                    "search",
                    Arc::new(JsonObject::default()),
                )])),
                block_tool_listing: Arc::new(AtomicBool::new(false)),
            },
            disconnect: disconnect.clone(),
        }))
        .await?,
    );
    client
        .initialize(
            InitializeRequestParams::new(
                ClientCapabilities::default(),
                Implementation::new("codex-test", "0.0.0-test"),
            )
            .with_protocol_version(ProtocolVersion::V_2025_06_18),
            /*timeout*/ None,
            Box::new(|_, _| async { Err(anyhow!("unexpected elicitation")) }.boxed()),
        )
        .await?;
    let view = previous
        .servers
        .get_mut("docs")
        .expect("test server should exist");
    let mut connected_client = view.connection.client().await?;
    connected_client.client = Arc::clone(&client);
    view.connection = Arc::new(McpServerConnection {
        identity: Some(reusable_server_identity("docs", &config, &runtime_context)),
        client: AsyncManagedClient {
            client: futures::future::ready(Ok(connected_client))
                .boxed()
                .shared(),

            server_capabilities: Arc::new(std::sync::Mutex::new(None)),

            tool_catalog_cache_context: None,
            startup_complete: Arc::new(std::sync::atomic::AtomicBool::new(true)),

            cancel_token: CancellationToken::new(),
        },
        startup_timeout: config
            .startup_timeout_sec
            .unwrap_or(DEFAULT_STARTUP_TIMEOUT),
        startup_trigger: None,
        _diagnostics_guard: LIVE_CONNECTIONS.track(),
    });

    assert!(!client.is_closed().await);
    disconnect.cancel();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !client.is_closed().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("closed MCP transport should be detected");

    let reconciled = reconcile_reusable_server(&previous, config, runtime_context).await;

    assert!(!previous.shares_test_connection_with(&reconciled, "docs"));
    Ok(())
}

#[tokio::test]
async fn reconciliation_reconnects_when_connection_identity_changes() {
    let runtime_context = reusable_server_runtime_context();
    let previous_config = reusable_server_config("http://127.0.0.1:1");
    let previous = manager_with_reusable_ready_server(
        &previous_config,
        &runtime_context,
        vec![create_test_tool("docs", "search")],
    )
    .await;

    let reconciled = reconcile_reusable_server(
        &previous,
        reusable_server_config("http://127.0.0.1:2"),
        runtime_context,
    )
    .await;

    assert!(!previous.shares_test_connection_with(&reconciled, "docs"));
}

#[tokio::test]
async fn reconciliation_reconnects_when_host_plugin_root_changes() {
    let runtime_context = reusable_server_runtime_context();
    let server_config = reusable_server_config("http://127.0.0.1:1");
    let original_root = PathUri::parse("file:///plugins/original").expect("valid plugin root URI");
    let replacement_root =
        PathUri::parse("file:///plugins/replacement").expect("valid plugin root URI");
    let mut previous = manager_with_reusable_ready_server(
        &server_config,
        &runtime_context,
        vec![create_test_tool("docs", "search")],
    )
    .await;
    let server = EffectiveMcpServer::from_host_config(server_config.clone());
    let resolved_environment = runtime_context.resolve_server_environment("docs", &server_config);
    let original_identity = McpServerConnectionIdentity::new(
        "docs",
        &server,
        Some(&original_root),
        OAuthCredentialsStoreMode::default(),
        AuthKeyringBackendKind::default(),
        McpOAuthRefreshMode::Legacy,
        &resolved_environment,
        &runtime_context,
        ElicitationCapability::default(),
        ClientMcpExtensions::default(),
        /*previous_identity*/ None,
    );
    Arc::get_mut(
        &mut previous
            .servers
            .get_mut("docs")
            .expect("test server should exist")
            .connection,
    )
    .expect("test server should have one connection owner")
    .identity = Some(original_identity);

    let codex_home = tempdir().expect("tempdir");
    let config_for_root = |root| {
        let mut config = crate::mcp::tests::test_mcp_config(codex_home.path().to_path_buf());
        let mut catalog = crate::ResolvedMcpCatalog::builder();
        catalog.register(crate::McpServerRegistration::from_plugin(
            "docs".to_string(),
            crate::McpPluginAttribution::new("docs@test".to_string(), "Docs".to_string())
                .with_host_root(root),
            /*plugin_order*/ 0,
            server_config.clone(),
        ));
        config.mcp_server_catalog = catalog.build();
        config
    };

    let unchanged = reconcile_reusable_server_with_mcp_config(
        &previous,
        "docs",
        server_config.clone(),
        runtime_context.clone(),
        config_for_root(original_root),
    )
    .await;
    assert!(previous.shares_test_connection_with(&unchanged, "docs"));

    let replacement_config = config_for_root(replacement_root);
    let replacement = reconcile_reusable_server_with_mcp_config(
        &unchanged,
        "docs",
        server_config,
        runtime_context,
        replacement_config,
    )
    .await;
    assert!(!unchanged.shares_test_connection_with(&replacement, "docs"));
}

#[tokio::test]
async fn view_only_changes_reuse_connection_and_preserve_the_old_step() {
    let runtime_context = reusable_server_runtime_context();
    let mut old_config = reusable_server_config("http://127.0.0.1:1");
    old_config.default_tools_approval_mode = Some(AppToolApproval::Prompt);
    let previous = Arc::new(
        manager_with_reusable_ready_server(
            &old_config,
            &runtime_context,
            vec![
                create_test_tool("docs", "search"),
                create_test_tool("docs", "write"),
            ],
        )
        .await,
    );
    let old_step = capture_binding(&previous).await;
    let old_call = old_step
        .prepare_call("docs", "search")
        .expect("old step should prepare search");

    let mut new_config = old_config;
    new_config.enabled_tools = Some(vec!["search".to_string()]);
    new_config.default_tools_approval_mode = Some(AppToolApproval::Approve);
    let reconciled =
        Arc::new(reconcile_reusable_server(previous.as_ref(), new_config, runtime_context).await);
    assert!(previous.shares_test_connection_with(&reconciled, "docs"));

    let new_step = capture_binding(&reconciled).await;
    let new_call = new_step
        .prepare_call("docs", "search")
        .expect("new step should prepare search");
    drop(previous);

    assert_eq!(
        old_step
            .tools()
            .iter()
            .map(|tool| tool.tool.name.to_string())
            .collect::<HashSet<_>>(),
        HashSet::from(["search".to_string(), "write".to_string()])
    );
    assert_eq!(old_call.tool_approval_mode(), AppToolApproval::Prompt);
    assert_eq!(
        new_step
            .tools()
            .iter()
            .map(|tool| tool.tool.name.to_string())
            .collect::<Vec<_>>(),
        vec!["search".to_string()]
    );
    assert_eq!(new_call.tool_approval_mode(), AppToolApproval::Approve);
}
