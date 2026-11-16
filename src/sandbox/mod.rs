//! Sandbox backends for running agents in isolated environments.

mod acknowledgement;
pub(crate) mod clipboard;
mod constant_time;
mod container;
pub mod docker_sbx;
pub mod freshness;
mod git_boundary;
pub mod guest;
pub(crate) mod host_exec_sandbox;
pub mod lima;
pub mod network_proxy;
pub(crate) mod notice;
pub(crate) mod pi;
pub mod rpc;
pub(crate) mod shims;
pub(crate) mod toolchain;

pub use container::CLAUDE_ONBOARDING_JSON;
pub use container::DEFAULT_IMAGE_REGISTRY;
pub use container::DOCKERFILE_BASE;
pub use container::KNOWN_AGENTS;
pub use container::SandboxPaths;
pub(crate) use container::build_docker_run_args;
pub use container::build_image;
pub use container::dockerfile_for_agent;
pub use container::ensure_image_ready;
pub(crate) use container::ensure_sandbox_config_dirs;
pub use container::pull_image;
pub(crate) use container::push_agent_home;
pub use container::stop_containers_for_handle;
pub(crate) use container::validate_git_metadata_boundary;
pub use container::wrap_for_container;
pub use docker_sbx::ensure_sandbox as ensure_docker_sbx_sandbox;
pub use docker_sbx::prepare_sandbox_removal as prepare_docker_sbx_removal;
pub use docker_sbx::wrap_for_docker_sbx;
pub use lima::ensure_vm_running as ensure_lima_vm;
pub use lima::wrap_for_lima;
pub(crate) fn proxy_env_vars(
    rpc_host: &str,
    proxy_port: u16,
    proxy_token: &str,
) -> Vec<(String, String)> {
    let proxy_url = format!("http://workmux:{}@{}:{}", proxy_token, rpc_host, proxy_port);
    let no_proxy = format!("localhost,127.0.0.1,{}", rpc_host);

    vec![
        ("HTTPS_PROXY".into(), proxy_url.clone()),
        ("HTTP_PROXY".into(), proxy_url.clone()),
        ("https_proxy".into(), proxy_url.clone()),
        ("http_proxy".into(), proxy_url),
        ("NO_PROXY".into(), no_proxy.clone()),
        ("no_proxy".into(), no_proxy),
        // Pass hostname (not IP literal) so the init script can resolve ALL
        // IPs and whitelist them all in iptables.
        ("WM_PROXY_HOST".into(), rpc_host.to_string()),
        ("WM_PROXY_PORT".into(), proxy_port.to_string()),
    ]
}

/// Where an agent's config directory is mounted under the guest home, or
/// `None` for agents whose config directory workmux doesn't mount.
fn agent_config_guest_subpath(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" => Some(".claude"),
        "copilot" => Some(".copilot"),
        "gemini" => Some(".gemini"),
        "agy" => Some(".gemini/antigravity-cli"),
        "codex" => Some(".codex"),
        "opencode" => Some(".local/share/opencode"),
        "pi" => Some(".pi/agent"),
        "omp" => Some(".omp/agent"),
        _ => None,
    }
}

/// Sanitize a project name for use in a sandbox's name.
///
/// Lowercases, replaces non-alphanumeric characters with hyphens,
/// collapses consecutive hyphens, and strips leading/trailing hyphens.
fn sanitize_name(name: &str, max_len: usize) -> String {
    let mut result = String::with_capacity(name.len());
    let mut prev_hyphen = false;

    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            result.push(c.to_ascii_lowercase());
            prev_hyphen = false;
        } else if !prev_hyphen {
            result.push('-');
            prev_hyphen = true;
        }
    }

    let trimmed = result.trim_matches('-');
    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else {
        trimmed[..max_len].trim_end_matches('-').to_string()
    }
}

/// Hash a key and return the first `len` hex characters (zero-padded).
fn hash_key(key: &str, len: usize) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    let hash = hasher.finish();
    let hex = format!("{:016x}", hash);
    hex[..len].to_string()
}
