use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// 单条路径内允许的符号链接展开次数（防环，超出后退回词法解析）。
const MAX_SYMLINK_EXPANSIONS: usize = 64;

/// 遍历用的路径步骤（`Component` 借用源路径，无法承载读取出的链接目标，
/// 故全部转为自有数据）。
enum Step {
    Root,
    Up,
    Name(OsString),
}

fn steps(path: &Path) -> VecDeque<Step> {
    path.components()
        .filter_map(|component| {
            Some(match component {
                Component::RootDir => Step::Root,
                Component::ParentDir => Step::Up,
                Component::Normal(name) => Step::Name(name.to_os_string()),
                Component::CurDir | Component::Prefix(_) => return None,
            })
        })
        .collect()
}

/// 逐分量 canonicalize：存在且为符号链接的分量按 `readlink` 实时解析
/// （相对目标续在当前父目录下、绝对目标从根重排），`..` 弹栈、`.` 忽略，
/// 不存在的后缀按词法拼接 —— 与 JDK `canonicalize`（已存在前缀
/// realpath 化 + 未知后缀词法化）语义一致。
pub fn canonicalize_like_java(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    let mut queue = steps(path);
    let mut expansions = 0usize;
    while let Some(step) = queue.pop_front() {
        match step {
            Step::Root => {
                if result.as_os_str().is_empty() {
                    result = PathBuf::from("/");
                }
            }
            Step::Up => {
                result.pop();
            }
            Step::Name(name) => {
                let candidate = result.join(&name);
                if expansions < MAX_SYMLINK_EXPANSIONS
                    && let Ok(link) = std::fs::read_link(&candidate)
                {
                    expansions += 1;
                    if link.is_absolute() {
                        result = PathBuf::new();
                    }
                    for part in steps(&link).into_iter().rev() {
                        queue.push_front(part);
                    }
                    continue;
                }
                result = candidate;
            }
        }
    }
    result
}

/// Java `File(base, child)` 的字符串拼接语义后 canonicalize。
/// `child` 中的 `\` 已由调用方统一为 `/`；绝对 child 仍落在 `base`
/// 之下（与 Java `resolve` 拼接行为一致，重复斜杠由分量遍历折叠）。
pub fn canonicalize_joined(base: &Path, child: &str) -> PathBuf {
    let mut joined = base.as_os_str().to_os_string();
    joined.push("/");
    joined.push(child);
    canonicalize_like_java(Path::new(&joined))
}

/// 解析归档条目路径并校验位于 `base`（已 canonical）之内，
/// 防 Zip Slip；`None` 表示拒绝。与原 `ArchiveExtractor.resolveSafe`
/// 判据一致：目标等于 base，或以 `base + "/"` 为字符串前缀。
pub fn resolve_safe(base: &Path, entry_name: &str) -> Option<PathBuf> {
    let normalized = entry_name.replace('\\', "/");
    let target = canonicalize_joined(base, &normalized);
    if target == base {
        return Some(target);
    }
    let mut prefix = base.as_os_str().to_os_string();
    prefix.push("/");
    target
        .as_os_str()
        .as_encoded_bytes()
        .starts_with(prefix.as_encoded_bytes())
        .then_some(target)
}

/// ecpkg 前缀剥离后的相对路径合法性：不得以 `/` 开头、
/// 任何分量不得为 `..`（与原 `RuntimeInstaller.extractEntry` 判据一致）。
pub fn is_safe_rel(rel: &str) -> bool {
    !rel.starts_with('/') && !rel.split('/').any(|component| component == "..")
}
