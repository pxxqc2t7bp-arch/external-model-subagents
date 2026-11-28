//! Cross-platform (macOS + Linux) binary discovery for external CLI harnesses without
//! hardcoded user paths or version numbers.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone, Debug)]
pub struct DiscoveredBins {
    pub codex: Option<PathBuf>,
    pub claude: Option<PathBuf>,
    pub trae: Option<PathBuf>,
    pub zcode_script: Option<PathBuf>,
    pub zcode_standalone: Option<PathBuf>,
    pub node: Option<PathBuf>,
}

static BINS: OnceLock<DiscoveredBins> = OnceLock::new();

pub fn bins() -> &'static DiscoveredBins {
    BINS.get_or_init(scan_all)
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
}

pub fn find_binary(name: &str, env_override: Option<&str>) -> Option<PathBuf> {
    let override_bin = env_override.and_then(env::var_os).map(PathBuf::from);
    find_binary_in(name, override_bin.as_deref(), &candidate_bin_dirs())
}

pub fn find_binary_in(name: &str, override_bin: Option<&Path>, dirs: &[PathBuf]) -> Option<PathBuf> {
    if let Some(path) = override_bin.filter(|p| is_executable(p)) {
        return std::path::absolute(path).ok();
    }
    if Path::new(name).components().count() > 1 && is_executable(Path::new(name)) {
        return std::path::absolute(name).ok();
    }
    for dir in dirs {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return std::path::absolute(candidate).ok();
        }
        #[cfg(windows)]
        if Path::new(name).extension().is_none() {
            for ext in ["exe", "cmd", "bat"] {
                let with_ext = dir.join(format!("{name}.{ext}"));
                if is_executable(&with_ext) {
                    return std::path::absolute(with_ext).ok();
                }
            }
        }
    }
    None
}

fn scan_all() -> DiscoveredBins {
    let codex_raw = find_binary("codex", Some("CODEX_BIN"));
    let codex = codex_raw.map(|p| unwrap_native_codex(&p).unwrap_or(p));
    let claude = find_binary("claude", Some("CLAUDE_BIN"));
    let trae = find_binary("traecli", Some("TRAE_BIN"))
        .or_else(|| find_binary("trae", None))
        .or_else(|| find_binary("trae-agent", None));
    let node = find_binary("node", Some("NODE_BIN"));

    let zcode_standalone = find_binary("zcode", Some("ZCODE_BIN"));
    let zcode_script = env::var_os("ZCODE_SCRIPT")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| {
            let mac_app = PathBuf::from("/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs");
            mac_app.is_file().then_some(mac_app)
        });

    DiscoveredBins {
        codex,
        claude,
        trae,
        zcode_script,
        zcode_standalone,
        node,
    }
}

/// When `codex` points to an npm Node wrapper script, locate the bundled native Rust binary
/// directly (`@openai/codex-*/vendor/*/bin/codex`) to save ~40 MB RAM and Node startup latency.
fn unwrap_native_codex(wrapper: &Path) -> Option<PathBuf> {
    let real = fs::canonicalize(wrapper).ok()?;
    // If already a large native Mach-O / ELF binary (> 1 MiB), use it directly.
    if fs::metadata(&real).ok()?.len() > 1024 * 1024 {
        return Some(real);
    }
    // Typical layout: <prefix>/lib/node_modules/@openai/codex/bin/codex.js
    let pkg_root = real.parent()?.parent()?;
    let nm = pkg_root.join("node_modules").join("@openai");
    for entry in fs::read_dir(&nm).ok()?.flatten() {
        let fname = entry.file_name();
        if fname.to_string_lossy().starts_with("codex-") {
            let vendor = entry.path().join("vendor");
            if let Ok(targets) = fs::read_dir(vendor) {
                for t in targets.flatten() {
                    let native = t.path().join("bin").join("codex");
                    if is_executable(&native) {
                        return Some(native);
                    }
                }
            }
        }
    }
    None
}

fn candidate_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(24);
    if let Some(path_var) = env::var_os("PATH") {
        dirs.extend(env::split_paths(&path_var));
    }
    let home = home_dir();
    dirs.push(home.join(".local/bin"));
    dirs.push(home.join(".local/share/mise/shims"));
    dirs.push(home.join(".volta/bin"));
    dirs.push(home.join(".bun/bin"));
    dirs.push(home.join(".cargo/bin"));

    // Scan versioned Node manager directories sorted newest-first.
    collect_versioned_bins(&home.join(".local/share/mise/installs/node"), "bin", &mut dirs);
    collect_versioned_bins(&home.join(".nvm/versions/node"), "bin", &mut dirs);
    collect_versioned_bins(
        &home.join(".local/share/fnm/node-versions"),
        "installation/bin",
        &mut dirs,
    );

    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs.push(PathBuf::from("/usr/bin"));
    dirs
}

fn collect_versioned_bins(root: &Path, sub: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut versions: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    versions.sort_by(|a, b| b.cmp(a));
    for v in versions.into_iter().take(4) {
        let bin = v.join(sub);
        if bin.is_dir() {
            out.push(bin);
        }
    }
}

#[cfg(unix)]
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub fn is_executable(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}
