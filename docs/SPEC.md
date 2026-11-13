# Riffle 작업지시서 (SPEC)

## 1. 목표

macOS에서 수 GB~수십 GB 크기의 텍스트 로그 파일을 다루는 데스크톱 앱.
기존 텍스트 에디터·Console 앱이 멈추는 크기의 파일을 **즉시 열고, 60fps로 스크롤하고, 빠르게 검색**하는 것이 핵심 가치다.

### 성능 목표 (Apple Silicon, 내장 SSD 기준)

| 항목                            | 목표                                      |
| ------------------------------- | ----------------------------------------- |
| 10GB 파일 열기 → 첫 화면 표시   | 300ms 이내 (인덱싱 완료를 기다리지 않음)  |
| 10GB 파일 전체 줄 인덱싱        | 10초 이내, 진행률 표시                    |
| 스크롤                          | 끊김 없는 60fps, 임의 위치 점프 즉시 반응 |
| 10GB 리터럴 문자열 검색         | 10초 이내, 결과는 찾는 즉시 스트리밍      |
| 앱 메모리 (OS 페이지 캐시 제외) | 200MB 이하                                |

### MVP 범위 밖 (이후 Phase)

CSV/JSON Lines 컬럼 뷰, 탭/다중 파일, 원격(SSH) 파일, 파일 편집. 편집은 범위에 넣지 않는다 (읽기 전용 뷰어).

## 2. 프로젝트 구조

```
riffle/
├── CLAUDE.md
├── docs/SPEC.md
├── Cargo.toml                  # workspace: members = ["crates/riffle-core", "src-tauri"]
├── crates/
│   └── riffle-core/            # 순수 Rust 라이브러리 (Tauri 의존성 없음)
│       ├── src/
│       │   ├── lib.rs
│       │   ├── source.rs       # 파일 열기, mmap / read_at 읽기
│       │   ├── index.rs        # 희소 줄 인덱스
│       │   ├── lines.rs        # 줄 범위 읽기, 디코딩, 긴 줄 자르기
│       │   ├── search.rs       # 검색 (리터럴/정규식), 취소
│       │   └── error.rs
│       ├── examples/
│       │   ├── gen_log.rs      # 테스트용 대용량 로그 생성기
│       │   └── cli.rs          # 열기/인덱싱/읽기/검색 수동 검증용 CLI
│       └── benches/            # criterion 벤치마크
├── src-tauri/                  # Tauri 앱 (얇은 IPC 래퍼)
│   ├── src/
│   │   ├── main.rs
│   │   ├── lib.rs
│   │   ├── commands.rs         # #[tauri::command] 정의
│   │   └── state.rs            # 열린 파일 핸들 관리
│   ├── capabilities/default.json
│   └── tauri.conf.json
├── src/                        # 프론트엔드
│   ├── App.tsx
│   ├── lib/
│   │   ├── ipc.ts              # invoke/Channel 타입 래퍼 (Rust 타입과 1:1)
│   │   └── lineCache.ts        # 줄 블록 LRU 캐시 + 프리페치
│   └── components/
│       ├── viewer/
│       │   ├── LogViewport.tsx # 가상 스크롤 본체
│       │   ├── Scrollbar.tsx   # 커스텀 스크롤바
│       │   └── Gutter.tsx      # 줄 번호
│       ├── search/SearchBar.tsx
│       └── StatusBar.tsx
└── testdata/                   # .gitignore
```

번들 식별자는 `com.yourname.riffle` 형태로 두고 추후 확정한다.

## 3. 코어 설계 (`riffle-core`)

### 3.1 파일 읽기

- (Phase 4 결정) 모든 읽기는 `std::os::unix::fs::FileExt::read_at` 기반으로 한다. mmap은 사용하지 않는다.
  - mmap 중인 파일이 외부에서 잘리면(truncate, 로그 로테이션) 해당 영역 접근 시 SIGBUS로 앱이 죽는다. read_at은 짧게 읽힐 뿐이므로 tail 여부와 무관하게 안전하다.
  - 측정 결과 인덱싱·검색은 read_at 더블 버퍼링이 mmap 페이지 폴트 방식보다 빨랐고(외장 SSD 238MB/s → 680MB/s), 줄 읽기는 블록당 수십~수백 KB라 pread로 충분하다(100줄 읽기 ≈23µs).
  - 읽기 방식은 `Source` 뒤에 숨겨 상위 코드가 신경 쓰지 않게 한다. 파일 길이는 tail로 증가할 수 있다.
- 인코딩: UTF-8을 가정하고, 잘못된 바이트는 `String::from_utf8_lossy`로 대체 문자 처리한다.
- 줄바꿈: `\n` 기준으로 나누고, 줄 끝의 `\r`은 표시 시 제거한다 (CRLF 지원).
- 마지막 줄에 개행이 없어도 한 줄로 인정한다.

### 3.2 희소 줄 인덱스

모든 줄의 시작 오프셋을 저장하면 줄 수가 많을 때 메모리가 폭증한다 (5천만 줄 × 8바이트 = 400MB). 대신 **K줄마다 하나씩만 오프셋을 기록**한다.

- `K = 1024` (상수, 조정 가능하게).
- `checkpoints[i]` = `i * K`번째 줄의 시작 바이트 오프셋.
- N번째 줄 찾기: `checkpoints[N / K]`로 점프 후 `memchr`로 최대 K-1개의 개행만 전진한다. 1024줄은 수백 KB 수준이라 마이크로초 단위로 끝난다.
- 바이트 오프셋 → 줄 번호 변환(검색 결과용): checkpoints 이진 탐색 후 구간 내 개행 수를 센다.
- 인덱스 생성은 병렬로 수행한다: 파일을 코어 수만큼 청크로 나눠 각 청크의 개행 위치를 `memchr`로 수집하고, 청크별 줄 수의 누적합으로 전역 줄 번호를 맞춘 뒤 checkpoints를 병합한다.
- **인덱싱 중에도 읽기가 가능해야 한다.** 파일 앞부분은 인덱싱 완료 전에도 표시할 수 있어야 한다 (첫 화면 300ms 목표). 인덱싱 진행 중에는 "현재까지 확인된 줄 수"를 노출한다.
- 진행률 콜백과 취소 플래그(`&AtomicBool`)를 받는다.

### 3.3 줄 읽기

```rust
pub struct Line {
    pub no: u64,              // 0부터 시작하는 줄 번호
    pub text: String,         // 표시용 텍스트 (긴 줄은 잘림)
    pub byte_len: u64,        // 원본 줄의 바이트 길이
    pub truncated: bool,
}
```

- 한 줄이 매우 길 수 있으므로(수 MB짜리 minified JSON 등) 표시 텍스트는 최대 `MAX_DISPLAY_BYTES = 16 * 1024`에서 자른다. UTF-8 문자 경계를 깨지 않게 자른다.
- `read_lines(start, count)`는 요청 범위가 파일 끝을 넘으면 있는 만큼만 반환한다.

### 3.4 검색

- 옵션: 리터럴 / 정규식, 대소문자 구분 여부.
- 리터럴은 `memchr::memmem`, 정규식은 `regex::bytes::Regex`를 사용한다.
- 파일을 **줄 경계에 맞춘** 청크로 나눠 `rayon`으로 병렬 검색한다 (청크 경계에서 매치가 잘리지 않게).
- (Phase 7) 매치는 **줄 단위**로 판정한다. 정규식 후보는 그 줄만 잘라 다시 검사하므로, 개행에 걸치는 패턴은 매치되지 않고 검색 결과와 하이라이트가 항상 일치한다. 빈 매치만 나오는 줄은 결과에서 제외하고 다음 줄로 건너뛴다.
- (Phase 7) 읽기 버퍼보다 긴 줄(개행 없는 수 GB 줄 포함)은 메모리를 버퍼 수준으로 유지하며 스트리밍 검사한다. 리터럴은 정확하지만, 정규식은 청크 사이를 64KB만 겹쳐 검사하므로 64KB보다 긴 매치가 청크 경계에 걸치면 놓칠 수 있다.
- 결과는 찾는 즉시 배치(예: 1,000건 단위)로 콜백에 전달한다. 결과는 줄 번호 기준으로 정렬된 순서를 보장한다 (청크 순서대로 내보내기).
- 새 검색이 시작되면 이전 검색은 취소 플래그로 중단한다.
- 결과 건수 상한(예: 1,000만 건)을 두고, 넘으면 중단하고 "결과가 너무 많음" 상태를 반환한다.
- 하이라이트: `read_lines`에 검색어가 주어지면 각 줄의 매치 범위를 함께 반환한다. **범위는 JS 문자열 인덱스와 맞도록 UTF-16 코드 유닛 기준으로 변환해서** 반환한다 (바이트 오프셋을 그대로 넘기면 한글에서 위치가 어긋난다).

### 3.5 다중 필터 (Phase 10)

필터는 "어떤 줄을 보여줄지"를 결정하고, 검색은 "보이는 줄 안에서만 찾기"(교집합은 Rust에서 계산)로 두 기능을 분리한다.

- 보이는 줄 = `level_ok ∧ include_ok ∧ ¬exclude_any`.
  - `level_ok`: 레벨 조건이 비어있지 않고 전체(5개)가 아닐 때만 적용. 줄의 레벨은 줄 앞 200바이트에서 정규식 `\b(ERROR|FATAL|WARN(?:ING)?|INFO|DEBUG|TRACE)\b` 첫 매치로 판정하고 `FATAL→ERROR`, `WARNING→WARN`으로 정규화한다. 레벨 토큰이 없는 줄은 레벨 조건 적용 시 제외된다.
  - `include_ok`: 켜진 포함 규칙이 없으면 true. `combine=Or`이면 하나라도 매치, `And`이면 모두 매치.
  - `exclude_any`: 켜진 제외 규칙 중 하나라도 매치하면 불통과.
- 매치 판정은 3.4와 같은 줄 단위 의미(줄 텍스트 = `\n` 제외, 끝 `\r` 제거)를 쓰고, 빈 매치는 매치로 치지 않는다. 켜진 규칙이 하나도 없고 레벨 조건도 없으면 "필터 없음"으로 취급한다(프론트는 `start_filter`를 호출하지 않는다).
- `FilterSet`은 켜진 규칙만 컴파일한다. 제외 규칙 판정은 줄마다 `Matcher`를 여러 번 돌리지 않고 `regex::bytes::RegexSet` 한 번으로 처리해 줄당 할당 없이 판정한다.
- 스캔 결과(보이는 줄 번호)는 `LineSet`(정렬·서로소 반열린 구간 목록 + 누적 개수)에 보관해 대량 결과를 압축한다. 구간 수가 `MAX_FILTER_RANGES`(1,000만)를 넘으면 `capped`로 중단한다.
- `LineSet` 연산: `push_line`(오름차순 추가, 인접 병합), `remove_line`(tail 재판정용), `contains`, `nth`(index→줄 번호), `rank`(줄 번호→index, 스크롤 위치 유지용), `len`, `range_count`.
- 규칙별 하이라이트: `read_lines`/`read_lines_at`에 `FilterSet`을 넘기면 각 줄의 `rule_highlights`에 `(UTF-16 start, end, spec.rules 인덱스)` 목록이 채워진다. 제외·비활성 규칙은 대상이 아니다.
- tail 연동: `Appended` 시 `old_line_count - 1` 줄부터 `new_line_count`까지를 `filter_lines_range`로 재판정한다(이전 마지막 줄은 이어쓰여 내용이 바뀌었을 수 있으므로 `LineSet`에서 먼저 제거). `TailEvent::Appended`의 `filter_count`로 프론트가 필터 보기 줄 수를 갱신한다.
- 프리셋: `app_data_dir/filter-presets.json`에 이름+조건을 원자적 저장한다(recent.json과 같은 방식). 같은 이름은 덮어쓰고, 파일이 없거나 깨져 있으면 빈 목록으로 처리한다.

### 3.6 테스트 필수 케이스

빈 파일, 개행 없는 마지막 줄, CRLF, 개행만 있는 파일, 16KB를 넘는 단일 줄, 잘못된 UTF-8 바이트, 청크 경계에 정확히 걸치는 줄, 청크 경계에 걸치는 검색 매치, 한글이 포함된 줄의 하이라이트 범위, K의 배수 번째 줄.

## 4. IPC 설계 (`src-tauri`)

모든 커맨드는 `Result<T, String>` 형태로 직렬화 가능한 에러를 반환한다.

| 커맨드               | 입력                                                                                                   | 출력                          | 비고                                                    |
| -------------------- | ------------------------------------------------------------------------------------------------------ | ----------------------------- | ------------------------------------------------------- |
| `open_file`          | `path`, `on_progress: Channel<IndexProgress>`                                                          | `FileInfo { id, path, size }` | 인덱싱을 백그라운드로 시작하고 즉시 반환                |
| `get_lines`          | `id`, `start`, `count`, `query?`, `spec?`                                                              | `Vec<Line>` (+ 하이라이트)    | count 상한 1,000. spec은 규칙 하이라이트용               |
| `search`             | `id`, `query`, `filter_id?`, `on_result: Channel<SearchEvent>`                                          | `search_id`                   | 이전 검색 자동 취소. filter_id 지정 시 필터와 교집합     |
| `cancel_search`      | `search_id`                                                                                            | `()`                          |                                                         |
| `validate_filter`    | `spec`                                                                                                 | `()`                          | FilterSet 컴파일 가능 여부 (오류에 규칙 번호 포함)       |
| `start_filter`       | `id`, `spec`, `on_event: Channel<SearchEvent>`                                                          | `filter_id`                   | 같은 파일의 이전 필터 자동 취소. 백그라운드 스캔         |
| `cancel_filter`      | `filter_id`                                                                                            | `()`                          |                                                         |
| `get_filter`         | `filter_id`                                                                                            | `FilterInfo`                  | 조건, 현재 줄 수, done/capped                            |
| `get_filter_lines`   | `id`, `filter_id`, `start_index`, `count`, `query?`, `spec?`                                            | `Vec<Line>`                   | 필터 결과 목록의 [start_index, +count) 줄. count ≤ 1,000 |
| `filter_index_of`    | `filter_id`, `line`                                                                                    | `u64`                         | line 이상인 첫 보이는 줄의 index (스크롤 위치 유지)      |
| `filter_density`     | `filter_id`, `bins`, `total_lines`                                                                     | `Vec<u32>`                    | 스크롤바 마커용 밀도                                     |
| `list_filter_presets`| -                                                                                                      | `Vec<FilterPreset>`           | 이름순                                                  |
| `save_filter_preset` | `name`, `spec`                                                                                         | `()`                          | 같은 이름 덮어쓰기. 유효하지 않은 spec은 거부            |
| `delete_filter_preset`| `name`                                                                                                | `()`                          | 없으면 에러                                              |
| `copy_lines`         | `id`, `start`, `end`                                                                                   | `()`                          | Rust에서 직접 클립보드에 기록 (대량 복사 대비)            |
| `close_file`         | `id`                                                                                                   | `()`                          | 인덱싱/검색/필터 취소 및 자원 해제                       |

- `IndexProgress { indexed_bytes, total_bytes, line_count, done }`
- `SearchEvent`는 `Progress { scanned_bytes, total_bytes, match_count }`, `Done { total, capped }`, `Cancelled`, `Error { message }` 중 하나. `start_filter`도 같은 이벤트 형식을 쓴다.
- `Line`에는 `highlights`(검색 매치, UTF-16)와 `rule_highlights`(규칙 매치 `[start, end, ruleIdx]`, UTF-16)가 있다.
- `FilterSpec { rules: [{ pattern, regex, caseSensitive, kind: "include"|"exclude", enabled, color }], combine: "and"|"or", levels: ["error"|"warn"|"info"|"debug"|"trace"] }`. `color`는 프론트 표시용이며 Rust는 저장만 한다.
- `TailEvent::Appended`에는 `filterCount`(활성 필터의 보이는 줄 수, 없으면 null)가 추가된다.
- (Phase 3 결정) 검색 결과 줄 번호 목록은 Rust에만 보관한다(최대 1,000만 건을 JS로 넘기지 않음). 프론트는 `search_nav`(다음/이전), `search_hit_at`, `search_density`(스크롤바 마커), `get_filtered_lines`(검색 필터 모드), `validate_query`로 필요한 만큼만 조회한다. 다중 필터의 보이는 줄도 `LineSet`으로 Rust에만 보관하고 `get_filter_lines`/`filter_index_of`/`filter_density`로 필요한 만큼만 조회한다.
- 열린 파일은 `State<AppState>` 안의 `HashMap<FileId, Arc<LogFile>>`로 관리한다.
- `capabilities/default.json`에는 실제로 쓰는 권한(dialog, clipboard 등)만 최소로 허용한다.
- 프론트엔드 `lib/ipc.ts`의 타입은 Rust 타입과 1:1로 맞춘다. 가능하면 `specta`/`tauri-specta`로 TS 타입을 자동 생성하는 방안을 Phase 2에서 검토하고, 도입 여부는 승인받는다.

## 5. 프론트엔드 설계

### 5.1 가상 스크롤

- 고정 높이 행(모노스페이스 폰트, 예: 20px). 가변 높이는 지원하지 않는다 (긴 줄은 한 줄로 자르고 가로 스크롤).
- 스크롤 상태는 픽셀이 아니라 **"맨 위에 보이는 줄 번호(topLine)"**로 관리한다.
- 마우스 휠/트랙패드 이벤트를 직접 받아 topLine을 갱신한다 (트랙패드의 부드러운 스크롤을 위해 소수점 단위 누적 → 행 높이 기준 서브픽셀 오프셋 적용).
- 커스텀 스크롤바: thumb 위치 = topLine / 전체 줄 수. 드래그 시 위치를 줄 번호로 역산해 즉시 점프한다. 인덱싱 중에는 전체 줄 수가 늘어나므로 thumb 크기와 위치가 갱신된다.

### 5.2 줄 캐시

- 512줄 단위 블록으로 `get_lines`를 요청하고 LRU 캐시(예: 최대 64블록)에 보관한다.
- 현재 화면 블록의 앞뒤 블록을 미리 요청한다.
- 아직 도착하지 않은 줄은 빈 플레이스홀더 행으로 그리고, 도착하면 채운다. 빠른 스크롤 중 오래된 요청 결과는 버린다.

### 5.3 선택과 복사

- 화면에 보이는 범위 안에서는 브라우저 기본 텍스트 선택을 허용한다.
- 화면을 벗어나는 큰 범위는 **줄 단위 선택 모델**로 처리한다: 줄 번호 클릭으로 선택, Shift+클릭으로 범위 확장. Cmd+C 시 범위가 크면 `copy_lines`로 Rust에서 직접 복사한다.

### 5.4 단축키

| 키                  | 동작                     |
| ------------------- | ------------------------ |
| Cmd+O               | 파일 열기                |
| Cmd+F               | 검색창 포커스            |
| Enter / Shift+Enter | 다음 / 이전 결과         |
| Cmd+L               | 줄 번호로 이동           |
| ↑↓, PageUp/PageDown | 스크롤                   |
| Cmd+↑ / Cmd+↓       | 파일 처음 / 끝           |
| Cmd+T               | tail 모드 토글 (Phase 4) |

### 5.5 UI 구성

상단 검색바, 중앙 뷰포트(줄 번호 거터 + 본문 + 커스텀 스크롤바), 하단 상태바(파일 경로, 크기, 줄 수, 인덱싱 진행률, 검색 결과 n/N, 현재 줄). 시스템 라이트/다크 모드를 따른다.

## 6. 단계별 작업 계획

각 Phase는 계획 제시 → 승인 → 구현 → 검증 → 커밋 순서로 진행한다.

### Phase 0. 프로젝트 셋업

- `pnpm create tauri-app`(React + TypeScript)으로 생성 후 Cargo workspace 구성, `crates/riffle-core` 추가.
- Tailwind CSS v4, shadcn/ui 설정. Prettier(tabWidth 4), ESLint, `typecheck`/`lint` 스크립트 추가.
- `.gitignore`에 `testdata/`, `target/`, `node_modules/` 등록.
- **완료 기준:** `pnpm tauri dev`로 빈 창이 뜨고, CLAUDE.md의 검증 명령이 모두 통과.

### Phase 1. 코어: 열기·인덱싱·줄 읽기

- `gen_log` 예제: 타임스탬프, 로그 레벨, 한글/영문 메시지, 가끔 매우 긴 줄이 섞인 로그를 지정 크기로 생성.
- `source.rs`, `index.rs`, `lines.rs` 구현 (3.1~3.3).
- `cli.rs` 예제: `open`, `lines <start> <count>`, `stats`(줄 수, 인덱싱 시간) 서브커맨드.
- 3.5의 테스트 케이스 중 검색 외 항목 작성, 인덱싱 벤치마크 작성.
- **완료 기준:** 10GB 생성 파일에 대해 CLI로 첫 줄 읽기가 즉시 되고, 인덱싱이 성능 목표 안에 끝나며, 임의 줄 번호 읽기 결과가 `sed -n 'Np'` 결과와 일치.

### Phase 4.5 gzip 압축 파일 (.gz)

- 판별: 파일 첫 2바이트가 `1f 8b`이면 gzip (확장자 무관). `Source` 뒤에 숨겨 상위 코드는 동일하게 동작한다.
- gzip은 랜덤 접근이 불가하므로 백그라운드에서 `flate2::bufread::MultiGzDecoder`(multi-member 지원, logrotate/`cat a.gz b.gz`)로 스트리밍 해제하여 임시 파일에 쓴다. 임시 파일은 `temp_dir()/riffle-gz/` 아래에 생성 직후 unlink하고 열린 핸들로만 읽고 쓴다 → 앱이 크래시해도 디스크에 남지 않는다. 앱 시작 시 `riffle_*.tmp` 패턴의 잔여 파일만 정리한다(오류 무시). `temp_dir()/riffle/`은 WKWebView가 쓰는 경로와 겹치므로 전용 이름 `riffle-gz`를 쓴다.
- 해제된 임시 파일은 "점점 커지는 파일"로 취급해 Phase 4의 `refresh()`/`LineIndex::extend`로 인덱스를 늘린다. 첫 화면은 해제 시작 직후부터 표시된다. 임시 파일은 경로가 없는 익명 소스이므로 `refresh()`는 Replaced/Truncated 판정을 건너뛰고 길이 증가만 Appended로 반영한다.
- 진행률은 압축 원본 기준(읽은 바이트/전체)과 해제된 바이트, 줄 수를 함께 보고한다. 디스크 여유 공간 사전 확인은 하지 않으며, 쓰기 오류(ENOSPC 등)나 손상된 gzip 발생 시 해제를 멈추고 그때까지 해제·인덱싱된 부분은 계속 볼 수 있게 한 뒤 에러를 사용자에게 알린다.
- gz 파일은 tail 모드를 지원하지 않는다 (원본 .gz는 append되지 않으므로).
- **완료 기준:** `cli <file.gz> stats`가 자동 판별해 해제+인덱싱 시간·첫 줄까지 시간·최대 RSS를 출력하고, 줄 수와 임의 줄 내용이 `gunzip -c` 결과와 일치.

### Phase 2. Tauri 연결과 가상 스크롤 뷰어

- `open_file`, `get_lines`, `close_file` 커맨드와 인덱싱 진행률 Channel.
- 파일 열기: Cmd+O 다이얼로그(`tauri-plugin-dialog`), 창에 드래그 앤 드롭.
- 5.1, 5.2 가상 스크롤과 줄 캐시, 줄 번호 거터, 상태바.
- **완료 기준:** 10GB 파일을 열면 300ms 안에 첫 화면이 보이고, 인덱싱 중에도 스크롤되며, 스크롤바를 끝까지 드래그하면 마지막 줄이 정확히 표시됨. 빠르게 스크롤해도 화면이 깨지거나 멈추지 않음.

### Phase 3. 검색

- 3.4 검색 구현 및 테스트, `search`/`cancel_search` 커맨드.
- 검색바(리터럴/정규식, 대소문자 토글), 결과 하이라이트, 다음/이전 이동, 결과 n/N 표시, 스크롤바에 결과 위치 마커(밀도 표시).
- 필터 모드: 매치된 줄만 보여주는 보기 (검색 결과 줄 번호 목록을 가상 스크롤의 데이터 소스로 사용).
- 잘못된 정규식은 입력 중 에러로 표시하고 검색을 시작하지 않음.
- **완료 기준:** 10GB 리터럴 검색이 목표 시간 안에 끝나고, 첫 결과는 1초 안에 표시. 한글 검색어의 하이라이트 위치가 정확함. 검색 중 새 검색어 입력 시 이전 검색이 즉시 중단됨.

### Phase 4. tail 모드 (실시간 추적)

- `notify` crate로 파일 변경 감시. 파일 끝에 추가된 부분만 인덱스에 반영.
- tail 모드 파일은 `read_at` 기반 읽기로 전환 (3.1 SIGBUS 주의사항).
- 파일 크기가 줄어들거나 inode가 바뀌면(로테이션) 사용자에게 알리고 처음부터 다시 연다.
- 사용자가 맨 아래에 있을 때만 자동 스크롤, 위로 스크롤하면 자동 스크롤 일시 정지.
- **완료 기준:** 초당 수천 줄이 추가되는 파일(테스트 스크립트로 생성)을 따라가도 UI가 끊기지 않고, 로그 로테이션 상황에서 앱이 죽지 않음.

### Phase 5. 편의 기능

- Cmd+L 줄 이동, 로그 레벨 색상(ERROR/WARN/INFO/DEBUG, 보이는 줄에만 프론트에서 적용), 5.3 줄 단위 선택과 복사, 최근 파일 목록, 긴 줄 전체 보기(잘린 줄 클릭 시 패널로 원문 표시), 폰트 크기 조절.
- macOS 네이티브 메뉴바 구성 (Tauri menu API).
- **완료 기준:** 5.4 단축키 전부 동작, 10만 줄 범위 복사가 UI 멈춤 없이 완료.

### Phase 6. 패키징

- `pnpm tauri build`로 `.app`/`.dmg` 생성, 앱 아이콘 적용.
- 파일 연결: `.log`, `.txt` 파일을 Finder의 "다음으로 열기"에서 선택 가능하게 등록.
- 코드 서명·공증은 Apple Developer 계정이 준비되면 별도 진행 (개인 사용 단계에서는 생략 가능).
- **완료 기준:** 빌드된 앱을 `/Applications`에 설치해 Finder에서 로그 파일을 열 수 있음.

### 이후 후보

CSV/JSON Lines 컬럼 뷰, 탭으로 여러 파일 열기, `.gz` 파일 지원, 여러 파일 시간순 병합 보기, 북마크.
