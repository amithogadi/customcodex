/// Registry-issued identity captured from the executor's authenticated relay connection.
pub(crate) struct ExecutorRegistration {
    pub(crate) environment_id: String,
    pub(crate) executor_registration_id: String,
}

impl ExecutorRegistration {
    pub(crate) fn new(environment_id: String, executor_registration_id: String) -> Option<Self> {
        if [&environment_id, &executor_registration_id]
            .iter()
            .any(|id| id.trim().is_empty() || id.len() > 256 || id.chars().any(char::is_control))
        {
            return None;
        }
        Some(Self {
            environment_id,
            executor_registration_id,
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ConnectionTransport {
    Relay,
    Stdio,
    WebSocket,
}
