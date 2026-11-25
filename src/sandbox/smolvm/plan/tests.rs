//! Tests for [`super::plan_launch`] and the invocations it builds.

use super::*;
use crate::config::ToolchainMode;
use crate::sandbox::smolvm::pack::{ContextSource, PackBuild, PackInput, PackToolchain, PackWhen};

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

pub(crate) fn claude_input(sandbox: &SandboxConfig) -> LaunchInput<'_> {
    LaunchInput {
        sandbox,
        agent: "claude",
        worktree: Path::new("/src/proj__worktrees/feat"),
        pane_cwd: Path::new("/src/proj__worktrees/feat"),
        command: "claude --dangerously-skip-permissions",
        machine: "wm-feat-42".into(),
        labels: vec![
            ("workmux".into(), "1".into()),
            ("wm-owner".into(), "7:99".into()),
        ],
        orphans: Vec::new(),
        user: GuestUser { uid: 501, gid: 20 },
        agent_config_dir: Some(PathBuf::from("/Users/me/.claude")),
        claude_config_dir: Some(PathBuf::from("/Users/me/.claude-sandbox-config")),
        rpc: Some(RpcEndpoint {
            host: "host.smolvm.internal".into(),
            port: 41234,
            token: "s3cret-token".into(),
        }),
        terminal_env: vec![("TERM".into(), "xterm-256color".into())],
        toolchain: DetectedToolchain::None,
        pack: pack_input(&[]),
        git: git_layout(),
        private_hosts: Vec::new(),
        confine_vmm: false,
        shims: None,
        extra_mounts: Vec::new(),
        git_identity: Vec::new(),
    }
}

/// A linked worktree `feat` of `/src/proj`, without submodules.
pub(crate) fn git_layout() -> GitLayout {
    GitLayout {
        common_dir: PathBuf::from("/src/proj/.git"),
        writable: ["objects", "refs", "logs", "worktrees/feat"]
            .iter()
            .map(|child| Path::new("/src/proj/.git").join(child))
            .collect(),
    }
}

/// The project's pack index holding `present`.
pub(crate) fn pack_input(present: &[&str]) -> PackInput {
    PackInput {
        prefix: "wm-pack-proj-0a1b2c3d".into(),
        dir: PathBuf::from("/state/smolvm-packs/0a1b2c3d4e5f6a7b"),
        present: present.iter().map(|p| p.to_string()).collect(),
        base_labels: vec![
            ("workmux".into(), "1".into()),
            ("wm-agent".into(), "pack".into()),
        ],
        toolchain: PackToolchain::none(),
        workmux_version: "9.9.9".into(),
        smolvm_version: "smolvm 1.36.1".into(),
        local_images: Default::default(),
    }
}

/// The pack a launch with `sandbox` would use, built or not.
fn pack_name(sandbox: &SandboxConfig) -> String {
    plan_launch(&claude_input(sandbox)).unwrap().pack.build.name
}

fn prepare_kinds(plan: &LaunchPlan) -> Vec<StepKind> {
    plan.prepare.iter().map(|step| step.kind).collect()
}

/// Whether `args` holds `pair` as adjacent arguments.
fn has_pair(args: &[String], pair: [&str; 2]) -> bool {
    args.windows(2).any(|w| w == pair)
}

#[test]
fn test_a_claude_pane_creates_starts_execs_and_deletes_its_machine() {
    let sandbox = SandboxConfig::default();
    let plan = plan_launch(&claude_input(&sandbox)).unwrap();
    let pack = &plan.pack.build.name;

    assert_eq!(plan.machine, "wm-feat-42");
    // The pack adds the guest user's passwd entry (see its "add the guest
    // user" step), so a pane runs no exec of its own before the agent's.
    assert_eq!(
        prepare_kinds(&plan),
        [StepKind::CreateMachine, StepKind::StartMachine]
    );
    let create = &plan.prepare[0].invocation.args;
    assert_eq!(
        create[..4],
        strings(&["machine", "create", "--name", "wm-feat-42"])
    );
    for label in ["workmux=1", "wm-owner=7:99", &format!("wm-pack={pack}")] {
        assert!(has_pair(create, ["--label", label]), "{label}: {create:?}");
    }
    let sidecar = format!("/state/smolvm-packs/0a1b2c3d4e5f6a7b/{pack}.smolmachine");
    assert!(has_pair(create, ["--from", &sidecar]), "{create:?}");
    assert_eq!(
        mounts(&plan),
        [
            "/src/proj__worktrees/feat:/src/proj__worktrees/feat",
            "/src/proj/.git:/src/proj/.git:ro",
            "/src/proj/.git/objects:/src/proj/.git/objects",
            "/src/proj/.git/refs:/src/proj/.git/refs",
            "/src/proj/.git/logs:/src/proj/.git/logs",
            "/src/proj/.git/worktrees/feat:/src/proj/.git/worktrees/feat",
            "/Users/me/.claude:/tmp/.claude",
            "/Users/me/.claude-sandbox-config:/tmp/.claude-sandbox-config",
        ]
    );
    assert_eq!(
        plan.prepare[1].invocation.args,
        strings(&["machine", "start", "--name", "wm-feat-42"])
    );

    let exec = &plan.exec.args;
    assert_eq!(
        exec[..4],
        strings(&["machine", "exec", "--name", "wm-feat-42"])
    );
    assert!(exec.contains(&"-it".to_string()), "{exec:?}");
    assert!(has_pair(exec, ["-u", "501:20"]), "{exec:?}");
    assert!(
        has_pair(exec, ["-w", "/src/proj__worktrees/feat"]),
        "{exec:?}"
    );
    assert_eq!(exec_env(&plan, "HOME").as_deref(), Some("/tmp"));
    assert_eq!(
        exec_env(&plan, "PATH").as_deref(),
        Some("/tmp/.local/bin:/usr/local/bin:/usr/bin:/bin")
    );
    assert_eq!(exec_env(&plan, "WM_SANDBOX_GUEST").as_deref(), Some("1"));
    assert_eq!(
        exec_env(&plan, "WM_RPC_HOST").as_deref(),
        Some("host.smolvm.internal")
    );
    assert_eq!(exec_env(&plan, "WM_RPC_PORT").as_deref(), Some("41234"));
    assert_eq!(exec_env(&plan, "TERM").as_deref(), Some("xterm-256color"));
    let script = exec.last().unwrap();
    assert_eq!(
        exec[exec.len() - 4..exec.len() - 1],
        strings(&["--", "sh", "-c"])
    );
    assert!(
        script.starts_with("ln -sf /tmp/.claude-sandbox-config/claude.json /tmp/.claude.json;"),
        "{script}"
    );
    assert!(
        script.ends_with("claude --dangerously-skip-permissions"),
        "{script}"
    );
    assert!(hot_mounts(&plan).is_empty(), "no shims, no hot mounts");

    assert_eq!(
        plan.delete.args,
        strings(&["machine", "delete", "--name", "wm-feat-42", "-f"])
    );
}

fn hot_mounts(plan: &LaunchPlan) -> Vec<Vec<String>> {
    plan.prepare
        .iter()
        .filter(|step| step.kind == StepKind::MountDirectory)
        .map(|step| step.invocation.args.clone())
        .collect()
}

fn exec_env(plan: &LaunchPlan, name: &str) -> Option<String> {
    plan.exec
        .args
        .windows(2)
        .filter(|pair| pair[0] == "-e")
        .find_map(|pair| pair[1].strip_prefix(&format!("{name}=")).map(String::from))
}

#[test]
fn test_shims_are_mounted_read_only_once_the_machine_runs_and_lead_path() {
    let sandbox = SandboxConfig::default();
    let mut input = claude_input(&sandbox);
    input.shims = Some(PathBuf::from("/cache/workmux/shims/shims-x/shims/bin"));
    let plan = plan_launch(&input).unwrap();

    assert_eq!(
        prepare_kinds(&plan),
        [
            StepKind::CreateMachine,
            StepKind::StartMachine,
            StepKind::MountDirectory
        ]
    );
    let added = hot_mounts(&plan);
    assert_eq!(
        added[0][..5],
        strings(&["machine", "mount", "add", "--name", "wm-feat-42"])
    );
    assert!(
        has_pair(
            &added[0],
            [
                "-v",
                "/cache/workmux/shims/shims-x/shims/bin:/tmp/.workmux-shims/bin:ro"
            ]
        ),
        "{added:?}"
    );
    assert_eq!(mounts(&plan).len(), 8, "shims aren't a boot mount");
    assert_eq!(
        exec_env(&plan, "PATH").unwrap(),
        "/tmp/.workmux-shims/bin:/tmp/.local/bin:/usr/local/bin:/usr/bin:/bin"
    );
}

fn hot(host: &str, guest: &str, read_only: bool) -> Mount {
    Mount {
        host: PathBuf::from(host),
        guest: PathBuf::from(guest),
        read_only,
    }
}

#[test]
fn test_extra_mounts_are_mounted_once_the_machine_runs_after_the_shims() {
    let sandbox = SandboxConfig::default();
    let mut input = claude_input(&sandbox);
    input.shims = Some(PathBuf::from("/cache/shims/bin"));
    input.extra_mounts = vec![
        hot("/Users/me/notes", "/Users/me/notes", true),
        hot("/Users/me/out", "/data/out", false),
    ];
    let plan = plan_launch(&input).unwrap();
    let added: Vec<_> = hot_mounts(&plan)
        .into_iter()
        .map(|args| args.last().unwrap().clone())
        .collect();
    assert_eq!(
        added,
        [
            "/cache/shims/bin:/tmp/.workmux-shims/bin:ro",
            "/Users/me/notes:/Users/me/notes:ro",
            "/Users/me/out:/data/out",
        ]
    );
    assert_eq!(mounts(&plan).len(), 8, "hot mounts aren't boot mounts");
}

#[test]
fn test_hot_mounts_dont_count_against_the_boot_limit() {
    let sandbox = SandboxConfig::default();
    let mut input = claude_input(&sandbox);
    // 11 boot mounts, the most smolvm boots with.
    input
        .git
        .writable
        .extend((0..3).map(|i| PathBuf::from(format!("/src/proj/.git/modules/m{i}/objects"))));
    input.shims = Some(PathBuf::from("/cache/shims/bin"));
    input.extra_mounts = (0..5)
        .map(|i| hot(&format!("/data/{i}"), &format!("/data/{i}"), true))
        .collect();
    let plan = plan_launch(&input).unwrap();
    assert_eq!(mounts(&plan).len(), 11);
    assert_eq!(hot_mounts(&plan).len(), 6);
}

#[test]
fn test_a_writable_extra_mount_over_git_metadata_is_an_error() {
    let sandbox = SandboxConfig::default();
    for guest in [
        "/src/proj/.git/hooks",
        "/src/proj",
        "/src/proj__worktrees/feat/.git",
    ] {
        let mut input = claude_input(&sandbox);
        input.extra_mounts = vec![hot("/Users/me/x", guest, false)];
        let error = plan_launch(&input).unwrap_err().to_string();
        assert!(
            error.contains("overlaps protected Git metadata"),
            "{guest}: {error}"
        );
        input.extra_mounts = vec![hot("/Users/me/x", guest, true)];
        assert!(plan_launch(&input).is_ok(), "{guest} read-only");
    }
}

#[test]
fn test_an_extra_mount_smolvm_cannot_parse_is_an_error() {
    let sandbox = SandboxConfig::default();
    let mut input = claude_input(&sandbox);
    input.extra_mounts = vec![hot("/Users/me/a:b", "/data/ab", true)];
    let error = plan_launch(&input).unwrap_err().to_string();
    assert!(error.contains("/Users/me/a:b"), "{error}");
}

fn identity() -> Vec<(String, String)> {
    [
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "user.name"),
        ("GIT_CONFIG_VALUE_0", "Ada Lovelace"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

#[test]
fn test_the_git_identity_reaches_the_agent_as_command_scope_config() {
    let sandbox = SandboxConfig::default();
    let mut input = claude_input(&sandbox);
    input.git_identity = identity();
    let plan = plan_launch(&input).unwrap();
    assert_eq!(exec_env(&plan, "GIT_CONFIG_COUNT").as_deref(), Some("1"));
    assert_eq!(
        exec_env(&plan, "GIT_CONFIG_VALUE_0").as_deref(),
        Some("Ada Lovelace")
    );
    assert!(
        !mounts(&plan).iter().any(|m| m.contains("gitconfig")),
        "no ~/.gitconfig mount"
    );
}

#[test]
fn test_sandbox_env_values_reach_the_agent_by_name_only() {
    let sandbox = sandbox("env:\n  ZED: last\n  API_URL: \"https://x.test/a b\"\n");
    let plan = plan_launch(&claude_input(&sandbox)).unwrap();
    assert_eq!(exec_passed(&plan), ["API_URL", "ZED"]);
    assert!(
        plan.exec
            .env
            .contains(&("API_URL".to_string(), "https://x.test/a b".to_string()))
    );
    assert!(
        plan.exec
            .env
            .contains(&("ZED".to_string(), "last".to_string()))
    );
    let argv = plan.exec.args.join(" ");
    assert!(!argv.contains("x.test") && !argv.contains("last"), "{argv}");
}

#[test]
fn test_orphaned_machines_are_deleted_before_the_panes_machine_is_created() {
    let sandbox = SandboxConfig::default();
    let mut input = claude_input(&sandbox);
    input.orphans = vec!["wm-old-1".into(), "wm-old-2".into()];
    let plan = plan_launch(&input).unwrap();
    assert_eq!(
        prepare_kinds(&plan),
        [
            StepKind::DeleteOrphan,
            StepKind::DeleteOrphan,
            StepKind::CreateMachine,
            StepKind::StartMachine
        ]
    );
    assert_eq!(
        plan.prepare[1].invocation.args,
        strings(&["machine", "delete", "--name", "wm-old-2", "-f"])
    );
}

#[test]
fn test_without_rpc_the_guest_gets_no_rpc_variables() {
    let sandbox = SandboxConfig::default();
    let mut input = claude_input(&sandbox);
    input.rpc = None;
    let plan = plan_launch(&input).unwrap();
    assert!(
        !plan.exec.args.iter().any(|a| a.starts_with("WM_")),
        "{:?}",
        plan.exec.args
    );
    assert!(plan.exec.env.is_empty());
}

#[test]
fn test_the_rpc_token_reaches_smolvm_by_name_only() {
    let sandbox = SandboxConfig::default();
    let plan = plan_launch(&claude_input(&sandbox)).unwrap();
    let invocations = plan
        .prepare
        .iter()
        .map(|step| &step.invocation)
        .chain([&plan.exec, &plan.delete]);
    for invocation in invocations {
        assert!(
            invocation.args.iter().all(|a| !a.contains("s3cret-token")),
            "{:?}",
            invocation.args
        );
    }
    assert!(has_pair(&plan.exec.args, ["-e", "WM_RPC_TOKEN"]));
    assert_eq!(
        plan.exec.env,
        [("WM_RPC_TOKEN".to_string(), "s3cret-token".to_string())]
    );
}

#[test]
fn test_resources_become_create_flags_only_when_set() {
    let cases: [(&str, Option<&[&str]>); 3] = [
        ("{}", None),
        ("cpus: 6\n", Some(&["--cpus", "6"])),
        (
            "cpus: 2\nmemory: 1.5GiB\n",
            Some(&["--cpus", "2", "--mem", "1536"]),
        ),
    ];
    for (yaml, expected) in cases {
        let sandbox = SandboxConfig {
            smolvm: serde_yaml::from_str(yaml).unwrap(),
            ..Default::default()
        };
        let args = plan_launch(&claude_input(&sandbox)).unwrap().prepare[0]
            .invocation
            .args
            .clone();
        let has_resources = args.iter().any(|a| a == "--cpus" || a == "--mem");
        match expected {
            None => assert!(!has_resources, "{yaml}: {args:?}"),
            Some(flags) => {
                let flags = strings(flags);
                assert!(
                    args.windows(flags.len()).any(|w| w == flags.as_slice()),
                    "{yaml}: {args:?}"
                );
            }
        }
    }
}

#[test]
fn test_the_pane_runs_in_its_own_directory_inside_the_worktree() {
    let sandbox = SandboxConfig::default();
    let mut input = claude_input(&sandbox);
    input.pane_cwd = Path::new("/src/proj__worktrees/feat/backend");
    let plan = plan_launch(&input).unwrap();
    let workdir = plan.exec.args.windows(2).find(|w| w[0] == "-w").unwrap();
    assert_eq!(workdir[1], "/src/proj__worktrees/feat/backend");

    input.pane_cwd = Path::new("/elsewhere");
    let error = plan_launch(&input).unwrap_err().to_string();
    assert!(error.contains("/elsewhere"), "{error}");
}

#[test]
fn test_a_codex_pane_mounts_codex_home_without_claudes_config() {
    let sandbox = SandboxConfig::default();
    let mut input = claude_input(&sandbox);
    input.agent = "codex";
    input.command = "codex";
    input.agent_config_dir = Some(PathBuf::from("/Users/me/.codex"));
    let plan = plan_launch(&input).unwrap();
    let create = &plan.prepare[0].invocation.args;
    assert!(
        plan.pack.build.steps[0]
            .invocation
            .args
            .contains(&"ghcr.io/raine/workmux-sandbox:codex".to_string())
    );
    assert_ne!(
        plan.pack.build.name,
        pack_name(&sandbox),
        "codex has a pack of its own"
    );
    assert!(create.contains(&"/Users/me/.codex:/home/user/.codex".to_string()));
    assert!(
        !create.iter().any(|a| a.contains("claude-sandbox-config")),
        "{create:?}"
    );
    assert!(
        plan.exec
            .args
            .contains(&"CODEX_HOME=/home/user/.codex".to_string())
    );
    assert_eq!(plan.exec.args.last().unwrap(), "codex");
}

/// A sandbox config from YAML.
fn sandbox(yaml: &str) -> SandboxConfig {
    serde_yaml::from_str(yaml).unwrap()
}

/// The variables the exec passes by name from smolvm's environment,
/// besides the RPC token.
fn exec_passed(plan: &LaunchPlan) -> Vec<String> {
    plan.exec
        .args
        .windows(2)
        .filter(|pair| pair[0] == "-e" && !pair[1].contains('=') && pair[1] != "WM_RPC_TOKEN")
        .map(|pair| pair[1].clone())
        .collect()
}

fn start_env(plan: &LaunchPlan) -> &[(String, String)] {
    &plan
        .prepare
        .iter()
        .find(|step| step.kind == StepKind::StartMachine)
        .unwrap()
        .invocation
        .env
}

fn deny(domains: &str) -> SandboxConfig {
    SandboxConfig {
        network: serde_yaml::from_str(&format!("policy: deny\nallowed_domains: {domains}\n"))
            .unwrap(),
        ..Default::default()
    }
}

/// The network flags of a plan's create: everything after the pack
/// sidecar up to the first mount or resource flag.
fn network_flags(plan: &LaunchPlan) -> Vec<String> {
    let create = &plan.prepare[0].invocation.args;
    let from = create.iter().position(|a| a == "--from").unwrap() + 2;
    create[from..]
        .iter()
        .take_while(|a| !["-v", "--cpus", "--mem"].contains(&a.as_str()))
        .cloned()
        .collect()
}

#[test]
fn test_network_policies_become_create_flags() {
    let gateway = [
        "--block-private",
        "--allow-private",
        "100.96.0.1/32",
        "--allow-private",
        "127.0.0.1/32",
    ];
    let virtio = ["--net", "--net-backend", "virtio-net"];
    let cases: Vec<(&str, SandboxConfig, Vec<&str>)> = vec![
        ("allow", SandboxConfig::default(), virtio.to_vec()),
        (
            "deny with hosts",
            deny("[api.anthropic.com, '*.githubusercontent.com']"),
            [
                &virtio[..],
                &gateway[..],
                &[
                    "--allow-host-pattern",
                    "api.anthropic.com",
                    "--allow-host-pattern",
                    "*.githubusercontent.com",
                ][..],
            ]
            .concat(),
        ),
        (
            "deny with no hosts allows only the gateway",
            deny("[]"),
            [
                &virtio[..],
                &gateway[..],
                &["--allow-cidr", "100.96.0.1/32"][..],
            ]
            .concat(),
        ),
        (
            "deny with no list at all",
            SandboxConfig {
                network: serde_yaml::from_str("policy: deny\n").unwrap(),
                ..Default::default()
            },
            [
                &virtio[..],
                &gateway[..],
                &["--allow-cidr", "100.96.0.1/32"][..],
            ]
            .concat(),
        ),
    ];
    for (name, sandbox, expected) in cases {
        let plan = plan_launch(&claude_input(&sandbox)).unwrap();
        assert_eq!(network_flags(&plan), strings(&expected), "{name}");
    }
}

#[test]
fn test_private_addresses_of_an_exact_host_that_allows_them_are_reopened() {
    let sandbox = deny("[{host: registry.corp.example, allow_private_ips: true}, public.example]");
    let mut input = claude_input(&sandbox);
    input.private_hosts = vec![
        (
            "registry.corp.example".into(),
            ["10.1.2.3", "93.184.216.34", "127.0.0.1", "fd00::7"]
                .iter()
                .map(|ip| ip.parse().unwrap())
                .collect(),
        ),
        // Listed without allow_private_ips: its private address stays closed.
        ("public.example".into(), vec!["10.9.9.9".parse().unwrap()]),
    ];
    let flags = network_flags(&plan_launch(&input).unwrap());
    let reopened: Vec<_> = flags
        .windows(2)
        .filter(|w| w[0] == "--allow-private")
        .map(|w| w[1].as_str())
        .collect();
    // Public addresses need no reopening, and loopback stays closed, as
    // on the container backend's proxy (the gateway's own CIDRs aside).
    assert_eq!(
        reopened,
        [
            "100.96.0.1/32",
            "127.0.0.1/32",
            "10.1.2.3/32",
            "fd00::7/128"
        ]
    );
    assert!(
        flags
            .windows(2)
            .any(|w| w == ["--allow-host-pattern", "registry.corp.example"])
    );
    assert!(
        flags
            .windows(2)
            .any(|w| w == ["--allow-host-pattern", "public.example"])
    );
}

#[test]
fn test_deny_refuses_ip_literals_even_past_config_validation() {
    let sandbox = deny("['10.0.0.1']");
    let error = plan_launch(&claude_input(&sandbox))
        .unwrap_err()
        .to_string();
    assert!(error.contains("10.0.0.1"), "{error}");
}

#[test]
fn test_deny_notes_where_smolvm_differs_from_the_container_proxy() {
    let plan = plan_launch(&claude_input(&SandboxConfig::default())).unwrap();
    assert!(plan.notes.is_empty(), "{:?}", plan.notes);

    let plan = plan_launch(&claude_input(&deny("[example.com]"))).unwrap();
    assert!(
        plan.notes.iter().any(|n| n.contains("every port")),
        "{:?}",
        plan.notes
    );
    assert!(!plan.notes.iter().any(|n| n.contains("max_connections")));

    let mut sandbox = deny("[example.com]");
    sandbox.network.max_connections = Some(16);
    let plan = plan_launch(&claude_input(&sandbox)).unwrap();
    assert!(
        plan.notes.iter().any(|n| n.contains("max_connections")),
        "{:?}",
        plan.notes
    );
}

#[test]
fn test_on_linux_hosts_machines_start_with_vmm_confinement() {
    // The pane's machine and the pack build's base machine alike.
    let sandbox = SandboxConfig::default();
    let start_envs = |confine_vmm: bool| {
        let plan = plan_launch(&LaunchInput {
            confine_vmm,
            ..claude_input(&sandbox)
        })
        .unwrap();
        let base = plan
            .pack
            .build
            .steps
            .iter()
            .find(|step| step.label == "start the base machine")
            .unwrap()
            .invocation
            .env
            .clone();
        [start_env(&plan).to_vec(), base]
    };
    for env in start_envs(false) {
        assert!(env.is_empty(), "{env:?}");
    }
    for env in start_envs(true) {
        assert_eq!(
            env,
            [
                ("SMOLVM_SECCOMP".to_string(), "enforce".to_string()),
                ("SMOLVM_LANDLOCK".to_string(), "enforce".to_string()),
            ]
        );
    }
}

fn mounts(plan: &LaunchPlan) -> Vec<&str> {
    let create = plan
        .prepare
        .iter()
        .find(|step| step.kind == StepKind::CreateMachine)
        .unwrap();
    create
        .invocation
        .args
        .windows(2)
        .filter(|w| w[0] == "-v")
        .map(|w| w[1].as_str())
        .collect()
}

#[test]
fn test_submodule_git_roots_get_writable_shares_under_the_read_only_git_dir() {
    let sandbox = SandboxConfig::default();
    let mut input = claude_input(&sandbox);
    input.git.writable.extend([
        PathBuf::from("/src/proj/.git/modules/lib/objects"),
        PathBuf::from("/src/proj/.git/modules/lib/refs"),
    ]);
    let plan = plan_launch(&input).unwrap();
    let mounts = mounts(&plan);
    let read_only = mounts
        .iter()
        .position(|m| *m == "/src/proj/.git:/src/proj/.git:ro")
        .unwrap();
    let module = mounts
        .iter()
        .position(|m| *m == "/src/proj/.git/modules/lib/refs:/src/proj/.git/modules/lib/refs")
        .unwrap();
    assert!(read_only < module, "{mounts:?}");
}

#[test]
fn test_more_mounts_than_smolvm_boots_with_is_an_error_naming_the_limit() {
    let sandbox = SandboxConfig::default();
    let mut input = claude_input(&sandbox);
    // The worktree, .git, four shares and two agent dirs make 8.
    input
        .git
        .writable
        .extend((0..4).map(|i| PathBuf::from(format!("/src/proj/.git/modules/m{i}/objects"))));
    let error = plan_launch(&input).unwrap_err().to_string();
    assert!(error.contains("at most 11 mounts"), "{error}");
    assert!(error.contains("needs 12 mounts"), "{error}");

    input.git.writable.pop();
    assert_eq!(mounts(&plan_launch(&input).unwrap()).len(), 11);
}

#[test]
fn test_a_path_smolvm_cannot_parse_as_a_mount_is_an_error() {
    let sandbox = SandboxConfig::default();
    let worktree = PathBuf::from("/src/proj__worktrees/a:b");
    let mut input = claude_input(&sandbox);
    input.worktree = &worktree;
    input.pane_cwd = &worktree;
    let error = plan_launch(&input).unwrap_err().to_string();
    assert!(error.contains("/src/proj__worktrees/a:b"), "{error}");
    assert!(error.contains("':'"), "{error}");
}

fn step_labels(plan: &LaunchPlan) -> Vec<&str> {
    plan.pack
        .build
        .steps
        .iter()
        .map(|step| step.label.as_str())
        .collect()
}

#[test]
fn test_excluded_files_is_refused_at_launch_and_other_container_settings_are_ignored() {
    let masked = sandbox("container:\n  excluded_files: [.env]\n");
    let error = plan_launch(&claude_input(&masked)).unwrap_err().to_string();
    assert!(
        error.contains("sandbox.container.excluded_files"),
        "{error}"
    );
    assert!(error.contains("smolvm"), "{error}");

    // As on Lima, settings for a container runtime don't apply.
    let runtime = sandbox(
        "container:\n  devices: [/dev/fuse]\n  group_add: [video]\n  cap_add: [NET_ADMIN]\n  \
         security_opt: [seccomp=unconfined]\n  oci_runtime: runsc\n",
    );
    plan_launch(&claude_input(&runtime)).unwrap();
}

#[test]
fn test_a_missing_pack_is_built_from_a_plain_base_before_the_machine_is_created() {
    let sandbox = SandboxConfig::default();
    let plan = plan_launch(&claude_input(&sandbox)).unwrap();
    let pack = &plan.pack.build;

    assert_eq!(plan.pack.when, PackWhen::BuildFirst);
    assert!(
        pack.name.starts_with("wm-pack-proj-0a1b2c3d-"),
        "{}",
        pack.name
    );
    assert_eq!(pack.name.len(), "wm-pack-proj-0a1b2c3d-".len() + 8);
    assert_eq!(
        pack.sidecar,
        PathBuf::from(format!(
            "/state/smolvm-packs/0a1b2c3d4e5f6a7b/{}.smolmachine",
            pack.name
        ))
    );
    assert_eq!(
        step_labels(&plan),
        [
            "create the base machine",
            "start the base machine",
            "add the guest user",
            "stop the base machine",
            "pack the base machine",
        ]
    );
    // Plain --net, and nothing a pack would carry into every machine:
    // no env, workdir, user, credentials or secret refs.
    let create = &pack.steps[0].invocation.args;
    assert_eq!(
        create[..4],
        strings(&["machine", "create", "--name", &pack.name])
    );
    assert!(has_pair(create, ["--label", "workmux=1"]), "{create:?}");
    assert!(has_pair(create, ["--label", "wm-agent=pack"]), "{create:?}");
    assert!(
        has_pair(create, ["--image", "ghcr.io/raine/workmux-sandbox:claude"]),
        "{create:?}"
    );
    assert!(create.contains(&"--net".to_string()), "{create:?}");
    for flag in [
        "-e",
        "-w",
        "-u",
        "-v",
        "--net-backend",
        "--secret-env",
        "--secret-keychain",
        "--credential",
    ] {
        assert!(!create.contains(&flag.to_string()), "{flag}: {create:?}");
    }
    assert!(pack.steps[0].invocation.env.is_empty());

    // Every pack, with a toolchain or not, gives the guest user a passwd
    // entry: Nix as the user needs it to realise the toolchain.
    let add_user = &pack.steps[2].invocation.args;
    assert_eq!(
        add_user[..6],
        strings(&["machine", "exec", "--name", &pack.name, "-u", "0:0"])
    );
    let script = add_user.last().unwrap();
    assert!(script.contains("getent passwd 501"), "{script}");
    assert!(
        script.contains("workmux-501:x:501:20:workmux:/tmp:/bin/sh"),
        "{script}"
    );
    assert!(script.contains("/etc/passwd"), "{script}");

    let packing = &pack.steps[4].invocation.args;
    assert_eq!(
        packing[..4],
        strings(&["pack", "create", "--from-vm", &pack.name])
    );
    let building = format!(
        "/state/smolvm-packs/0a1b2c3d4e5f6a7b/.{}.building",
        pack.name
    );
    assert!(has_pair(packing, ["-o", &building]), "{packing:?}");
    assert_eq!(
        pack.delete_base.args,
        strings(&["machine", "delete", "--name", &pack.name, "-f"])
    );
}

#[test]
fn test_a_locally_built_image_is_packed_from_its_saved_archive() {
    let sandbox = SandboxConfig::default();
    let registry = plan_launch(&claude_input(&sandbox)).unwrap().pack.build;
    let mut input = claude_input(&sandbox);
    let archive = PathBuf::from("/state/smolvm-images/claude.tar");
    input.pack.local_images.insert(
        crate::sandbox::smolvm::pack::image_slot("ghcr.io/raine/workmux-sandbox:claude"),
        archive.clone(),
    );
    let local = plan_launch(&input).unwrap().pack.build;

    let image_arg = |build: &PackBuild| {
        let args = &build.steps[0].invocation.args;
        let at = args.iter().position(|arg| arg == "--image").unwrap();
        args[at + 1].clone()
    };
    assert_eq!(image_arg(&local), "/state/smolvm-images/claude.tar");
    assert_eq!(image_arg(&registry), "ghcr.io/raine/workmux-sandbox:claude");
    assert_eq!(
        local.current_marker, registry.current_marker,
        "the archive stands in for the image, in the image's slot"
    );

    // Another agent's image isn't the archive's.
    let mut codex = claude_input(&sandbox);
    codex.agent = "codex";
    codex.pack = input.pack.clone();
    let codex = plan_launch(&codex).unwrap().pack.build;
    assert_eq!(image_arg(&codex), "ghcr.io/raine/workmux-sandbox:codex");
}

#[test]
fn test_a_present_pack_is_used_as_is() {
    let sandbox = SandboxConfig::default();
    let name = pack_name(&sandbox);
    let mut input = claude_input(&sandbox);
    input.pack = pack_input(&["wm-pack-proj-0a1b2c3d-00000000", &name]);
    let plan = plan_launch(&input).unwrap();
    assert_eq!(plan.pack.when, PackWhen::Present);
    assert_eq!(plan.pack.build.name, name);
    assert!(
        plan.prepare[0]
            .invocation
            .args
            .contains(&plan.pack.build.sidecar.to_string_lossy().into_owned())
    );
}

#[test]
fn test_a_changed_input_names_a_new_pack_which_is_built() {
    let base = SandboxConfig::default();
    let current = pack_name(&base);
    let changes: [(&str, SandboxConfig); 3] = [
        (
            "provision",
            SandboxConfig {
                smolvm: serde_yaml::from_str("provision: apt-get install -y tree\n").unwrap(),
                ..Default::default()
            },
        ),
        (
            "image",
            SandboxConfig {
                image: Some("alpine".into()),
                ..Default::default()
            },
        ),
        (
            "toolchain mode",
            SandboxConfig {
                toolchain: Some(ToolchainMode::Off),
                ..Default::default()
            },
        ),
    ];
    for (what, sandbox) in changes {
        let mut input = claude_input(&sandbox);
        input.pack = pack_input(&[&current]);
        let plan = plan_launch(&input).unwrap();
        assert_ne!(plan.pack.build.name, current, "{what}");
        assert_eq!(plan.pack.when, PackWhen::BuildFirst, "{what}");
    }

    // The default branch's toolchain files, workmux's version and smolvm's
    // (a newer smolvm may not read an older one's pack) too.
    let mut input = claude_input(&base);
    input.pack = pack_input(&[&current]);
    input.pack.toolchain = PackToolchain {
        kind: DetectedToolchain::Flake,
        files: vec![("flake.nix".into(), b"{ }".to_vec())],
        source: None,
    };
    assert_ne!(plan_launch(&input).unwrap().pack.build.name, current);
    let mut input = claude_input(&base);
    input.pack = pack_input(&[&current]);
    input.pack.workmux_version = "10.0.0".into();
    assert_ne!(plan_launch(&input).unwrap().pack.build.name, current);
    let mut input = claude_input(&base);
    input.pack = pack_input(&[&current]);
    input.pack.smolvm_version = "smolvm 1.37.0".into();
    let plan = plan_launch(&input).unwrap();
    assert_ne!(plan.pack.build.name, current);
    assert_eq!(plan.pack.when, PackWhen::BuildFirst);
}

/// A flake toolchain of `files` at commit `c0ffee` of `/src/proj`.
fn toolchain_at_commit(files: &[(&str, &str)]) -> PackToolchain {
    PackToolchain {
        kind: DetectedToolchain::Flake,
        files: files
            .iter()
            .map(|(name, body)| (name.to_string(), body.as_bytes().to_vec()))
            .collect(),
        source: Some(ContextSource {
            repo: PathBuf::from("/src/proj"),
            commit: "c0ffee".into(),
        }),
    }
}

#[test]
fn test_the_base_machine_mounts_the_pack_context_read_only_and_no_step_carries_files() {
    let sandbox = SandboxConfig::default();
    let mut input = claude_input(&sandbox);
    input.pack.toolchain = toolchain_at_commit(&[
        ("flake.nix", "{ outputs = _: { flake-marker = 1; }; }"),
        ("flake.lock", "lock-marker"),
    ]);
    let plan = plan_launch(&input).unwrap();
    let pack = &plan.pack.build;

    let staging = PathBuf::from(format!(
        "/state/smolvm-packs/0a1b2c3d4e5f6a7b/.{}.context",
        pack.name
    ));
    let context = pack
        .context
        .as_ref()
        .expect("a toolchain pack has a context");
    assert_eq!(context.staging, staging);
    assert_eq!(context.source.repo, PathBuf::from("/src/proj"));
    assert_eq!(context.source.commit, "c0ffee");
    let create = &pack.steps[0].invocation.args;
    assert!(
        create.windows(2).any(|w| w
            == [
                "-v".to_string(),
                format!("{}:/var/lib/workmux/pack-context:ro", staging.display())
            ]),
        "{create:?}"
    );
    for step in &pack.steps {
        let args = step.invocation.args.join(" ");
        for marker in ["flake-marker", "lock-marker", "base64"] {
            assert!(!args.contains(marker), "{}: {args}", step.label);
        }
    }

    // A pack without a toolchain has no context and mounts nothing.
    let plain = plan_launch(&claude_input(&sandbox)).unwrap().pack.build;
    assert!(plain.context.is_none());
    assert!(!plain.steps[0].invocation.args.contains(&"-v".to_string()));
}

#[test]
fn test_provision_runs_as_root_and_the_default_branchs_shell_is_realised_as_the_user() {
    let sandbox = SandboxConfig {
        smolvm: serde_yaml::from_str("provision: apt-get install -y tree\n").unwrap(),
        ..Default::default()
    };
    let mut input = claude_input(&sandbox);
    input.pack.toolchain = toolchain_at_commit(&[("flake.nix", "{ }"), ("flake.lock", "{}")]);
    let plan = plan_launch(&input).unwrap();
    let pack = &plan.pack.build;
    assert_eq!(
        step_labels(&plan),
        [
            "create the base machine",
            "start the base machine",
            "run provision",
            "add the guest user",
            "install Nix",
            "realise the default branch's nix develop shell",
            "stop the base machine",
            "pack the base machine",
        ]
    );
    // Provision runs after the base machine starts and before the guest
    // user is added, so a user provision creates for the uid is kept, not
    // refused as a duplicate.
    let provision = &pack.steps[2].invocation.args;
    assert!(has_pair(provision, ["-u", "0:0"]), "{provision:?}");
    assert_eq!(provision.last().unwrap(), "apt-get install -y tree");
    let install = pack.steps[4].invocation.args.join(" ");
    assert!(install.contains("-u 0:0"), "{install}");
    assert!(
        install.contains("install.determinate.systems/nix"),
        "{install}"
    );
    assert!(!install.contains("get.jetify.com"), "{install}");
    assert!(install.contains("chown -R 501:20 /nix"), "{install}");
    let realise = &pack.steps[5].invocation.args;
    assert!(
        realise.windows(2).any(|w| w == ["-u", "501:20"]),
        "{realise:?}"
    );
    // HOME stays the panes' /tmp: the passwd entry, not a HOME of its own,
    // is what lets Nix run as the user.
    assert!(
        realise.windows(2).any(|w| w == ["-e", "HOME=/tmp"]),
        "{realise:?}"
    );
    // A writable copy of the pack context in the guest's /tmp, which boot
    // wipes, so the copy never reaches the pack.
    let script = realise.last().unwrap();
    assert!(
        script.contains("cp -R /var/lib/workmux/pack-context /tmp/workmux-pack-context"),
        "{script}"
    );
    assert!(
        script.ends_with("nix develop path:/tmp/workmux-pack-context --command true"),
        "{script}"
    );
    assert!(pack.steps[5].warm_up, "the warm-up is best effort");
    assert!(
        pack.steps.iter().filter(|s| s.warm_up).count() == 1,
        "only the warm-up is"
    );
}

#[test]
fn test_a_devbox_toolchain_installs_devbox_and_its_packages() {
    let sandbox = SandboxConfig::default();
    let mut input = claude_input(&sandbox);
    input.pack.toolchain = PackToolchain {
        kind: DetectedToolchain::Devbox,
        ..toolchain_at_commit(&[("devbox.json", "{}")])
    };
    let plan = plan_launch(&input).unwrap();
    let step = |label: &str| {
        plan.pack
            .build
            .steps
            .iter()
            .find(|s| s.label == label)
            .unwrap_or_else(|| panic!("{label}: {:?}", step_labels(&plan)))
    };
    let realise = step("realise the default branch's devbox shell");
    let args = &realise.invocation.args;
    assert!(has_pair(args, ["-u", "501:20"]), "{args:?}");
    let script = args.last().unwrap();
    assert!(
        script.contains("cd /tmp/workmux-pack-context && devbox install"),
        "{script}"
    );
    assert!(realise.warm_up);
    // Boot wipes /tmp, the guest's HOME, and Nix's lazy trees keep a flake
    // input only in Nix's cache, so the warm-up caches where the pack keeps
    // it.
    assert!(
        has_pair(args, ["-e", "XDG_CACHE_HOME=/var/cache/workmux-toolchain"]),
        "{args:?}"
    );

    let install = step("install Nix and Devbox");
    let script = install.invocation.args.last().unwrap();
    assert!(
        script.contains("chown -R 501:20 /var/cache/workmux-toolchain"),
        "{script}"
    );
    // Devbox's launcher downloads the real binary into $HOME/.cache, which
    // boot wipes, so each pane fetched it from github.com again. The pack
    // runs the launcher once, in a throwaway HOME, and keeps the binary it
    // fetched in the launcher's place.
    for fragment in [
        "https://get.jetify.com/devbox",
        "HOME=$launcher_home DEVBOX_LAUNCHER_DEBUG=1 /usr/local/bin/devbox",
        "install -m 0755 \"$devbox_bin\" /usr/local/bin/devbox",
        "rm -rf \"$launcher_home\"",
    ] {
        assert!(script.contains(fragment), "{fragment}: {script}");
    }
}

#[test]
fn test_each_pane_enters_its_own_worktrees_toolchain() {
    let sandbox = SandboxConfig::default();
    let cases = [
        (
            DetectedToolchain::None,
            "claude --dangerously-skip-permissions",
        ),
        (
            DetectedToolchain::Flake,
            "nix develop --command bash -c 'claude --dangerously-skip-permissions'",
        ),
    ];
    for (toolchain, expected) in cases {
        let mut input = claude_input(&sandbox);
        input.pack.toolchain = toolchain_at_commit(&[("flake.nix", "{}")]);
        input.toolchain = toolchain.clone();
        let plan = plan_launch(&input).unwrap();
        let command = plan.exec.args.last().unwrap();
        assert!(command.ends_with(expected), "{toolchain:?}: {command}");
        let has_nix_path = plan
            .exec
            .args
            .iter()
            .any(|a| a.starts_with("PATH=/nix/var/nix/profiles/default/bin:"));
        assert_eq!(has_nix_path, toolchain != DetectedToolchain::None);
        // The warm-up's caches, which hold the default branch's inputs.
        let cache = (toolchain != DetectedToolchain::None)
            .then(|| "/var/cache/workmux-toolchain".to_string());
        assert_eq!(exec_env(&plan, "XDG_CACHE_HOME"), cache, "{toolchain:?}");
    }
    // A toolchain the default branch lacks caches under HOME: the pack
    // makes its cache directory only with a toolchain of its own, and the
    // guest user couldn't make it under /var/cache.
    let mut input = claude_input(&sandbox);
    input.toolchain = DetectedToolchain::Devbox;
    let plan = plan_launch(&input).unwrap();
    assert!(
        plan.exec.args.last().unwrap().contains("devbox run -c"),
        "{:?}",
        plan.exec.args
    );
    assert_eq!(exec_env(&plan, "XDG_CACHE_HOME"), None);
}
