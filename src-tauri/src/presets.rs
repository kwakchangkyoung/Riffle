// 필터 프리셋 저장 (app_data_dir/filter-presets.json)
// recent.json과 같은 방식: 전역 Mutex 직렬화 + 임시 파일 rename 원자적 저장.
// 파일이 없거나 JSON이 깨져 있으면 빈 목록으로 처리한다.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::commands::FilterPresetDto;

/// 프리셋 읽기-수정-쓰기를 직렬화하는 전역 락
static PRESETS_LOCK: Mutex<()> = Mutex::new(());

/// 프리셋 관련 읽기-수정-쓰기를 하나의 락으로 직렬화한다.
pub(crate) fn with_presets_lock<R>(f: impl FnOnce() -> R) -> R {
    let _guard = PRESETS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    f()
}

/// filter-presets.json 파일 경로
fn presets_file_path(dir: &Path) -> PathBuf {
    dir.join("filter-presets.json")
}

/// 프리셋 파일을 읽어 목록으로 반환한다.
/// 파일이 없거나 JSON 파싱에 실패하면 빈 목록.
fn read_presets_file(path: &Path) -> Vec<FilterPresetDto> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// 목록을 프리셋 파일에 원자적으로 저장한다.
/// 임시 파일에 쓴 뒤 rename으로 교체해 중간에 죽어도 파일이 깨지지 않게 한다.
fn write_presets_file(path: &Path, list: &[FilterPresetDto]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("프리셋 디렉터리 생성 실패 ('{}'): {e}", dir.display()))?;
    }
    let json = serde_json::to_string(list).map_err(|e| format!("프리셋 직렬화 실패: {e}"))?;
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);
    std::fs::write(&tmp_path, json)
        .map_err(|e| format!("프리셋 임시 저장 실패 ('{}'): {e}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("프리셋 저장 실패 ('{}'): {e}", path.display())
    })
}

/// 저장된 프리셋 목록을 이름순으로 반환한다.
pub fn list_presets(dir: &Path) -> Result<Vec<FilterPresetDto>, String> {
    let path = presets_file_path(dir);
    let mut list = read_presets_file(&path);
    list.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(list)
}

/// name의 프리셋을 저장한다. 같은 이름이 있으면 덮어쓴다.
/// 목록은 이름순으로 정렬해 저장한다.
pub fn save_preset(
    dir: &Path,
    name: &str,
    spec: crate::commands::FilterSpecDto,
) -> Result<(), String> {
    let path = presets_file_path(dir);
    let mut list = read_presets_file(&path);
    match list.iter_mut().find(|p| p.name == name) {
        Some(p) => p.spec = spec,
        None => list.push(FilterPresetDto {
            name: name.to_string(),
            spec,
        }),
    }
    list.sort_by(|a, b| a.name.cmp(&b.name));
    write_presets_file(&path, &list)
}

/// name의 프리셋을 삭제한다. 없으면 Err.
pub fn delete_preset(dir: &Path, name: &str) -> Result<(), String> {
    let path = presets_file_path(dir);
    let mut list = read_presets_file(&path);
    let before = list.len();
    list.retain(|p| p.name != name);
    if list.len() == before {
        return Err(format!("프리셋 '{name}'을(를) 찾을 수 없습니다."));
    }
    write_presets_file(&path, &list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{FilterRuleDto, FilterSpecDto};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "riffle_preset_test_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spec(pattern: &str) -> FilterSpecDto {
        FilterSpecDto {
            rules: vec![FilterRuleDto {
                pattern: pattern.to_string(),
                regex: false,
                case_sensitive: true,
                kind: "include".to_string(),
                enabled: true,
                color: "#ff0000".to_string(),
            }],
            combine: "or".to_string(),
            levels: Vec::new(),
        }
    }

    #[test]
    fn test_preset_save_list_overwrite_delete() {
        let dir = temp_dir("crud");
        let path = presets_file_path(&dir);

        // 빈 디렉터리 → 빈 목록
        assert!(list_presets(&dir).unwrap().is_empty());

        save_preset(&dir, "beta", spec("B")).unwrap();
        save_preset(&dir, "alpha", spec("A")).unwrap();

        let list = list_presets(&dir).unwrap();
        assert_eq!(list.len(), 2);
        // 이름순 정렬
        assert_eq!(list[0].name, "alpha");
        assert_eq!(list[1].name, "beta");
        assert_eq!(list[0].spec.rules[0].pattern, "A");

        // 같은 이름 덮어쓰기
        save_preset(&dir, "alpha", spec("A2")).unwrap();
        let list = list_presets(&dir).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].spec.rules[0].pattern, "A2");

        // 삭제
        delete_preset(&dir, "beta").unwrap();
        let list = list_presets(&dir).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "alpha");

        // 없는 이름 삭제 → Err
        assert!(delete_preset(&dir, "none").is_err());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_preset_corrupt_file_returns_empty() {
        let dir = temp_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = presets_file_path(&dir);
        std::fs::write(&path, b"{not valid json!!!").unwrap();

        // 손상된 파일은 빈 목록으로 처리된다
        assert!(list_presets(&dir).unwrap().is_empty());

        // 손상 파일 위에 저장하면 정상 목록으로 복구된다
        save_preset(&dir, "ok", spec("x")).unwrap();
        let list = list_presets(&dir).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "ok");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
