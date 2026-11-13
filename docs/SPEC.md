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
- (Phase 12) 원격 파일은 `RandomRead` trait(동기 random-access, 구현체가 내부에서 비동기 처리) 뒤에 숨긴다. `Source`는 로컬 File / 익명 File(gz 임시) / `Arc<dyn RandomRead>` 세 종류를 가지며, 인덱싱·검색·필터의 read_at 파이프라인은 그대로 재사용한다. 구현체(SFTP over ssh)는 riffle-remote가 제공한다. `refresh()`는 stat_len 증가→Appended, 감소→Truncated로 판정하고 inode가 없어 Replaced는 판정하지 않는다. 원격 .gz는 `open_gzip_remote`가 순차 읽기로 기존 임시파일 해제 경로에 태운다.
- (Phase 13) S3 객체도 같은 `RandomRead` 뒤에 숨긴다 (`S3Pool`, riffle-remote). `s3://버킷/키` URI를 열며, 인증은 AWS 기본 자격 증명 체인(프로필·SSO·`aws login`·환경 변수)을 쓰고 Riffle은 자격 증명을 저장하지 않는다. 읽기 전용 API(HeadObject, GetObject Range, ListObjectsV2, ListBuckets, HeadBucket)만 호출한다. 큰 읽기(4MiB 이상)는 8MiB 단위 Range GET 을 최대 16개 동시에 보내고, 작은 읽기는 1MiB 블록 LRU 캐시(최대 64MiB)를 거친다. 버킷 리전은 HeadBucket/리다이렉트 응답의 `x-amz-bucket-region` 헤더로 자동 결정해 버킷별로 캐시한다. 객체는 불변이라고 가정하므로 tail은 비활성화한다.

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

### 3.6 컬럼 뷰 (CSV / JSON Lines, Phase 16)

구조화된 텍스트(CSV, JSON Lines)를 컬럼 단위로 해석해 표시·필터링한다. **한 줄 = 한 행**이며, CSV의 따옴표 안 줄바꿈(다중 줄 레코드)은 지원하지 않는다(로그 뷰어 범위). 정렬은 지원하지 않는다(수천만 줄 전역 정렬은 범위 밖).

- 형식 감지 `detect_columns(&LogFile) -> Columns`: 파일 앞부분 표본(최대 500줄 또는 1MB)으로 판정하고, 감지 결과는 파일 단위로 고정한다. 판정 순서는 JSON Lines → CSV → Plain이다.
  - **JSON Lines**: 비어있지 않은 표본 줄의 90% 이상이 `{`로 시작하고 JSON 객체로 파싱될 때. `names`는 표본 객체 최상위 키의 합집합이며 문서 처음 등장 순서를 유지한다. 최대 64개까지만 컬럼으로 쓰고, 넘치는 키가 있으면 마지막에 `_extra` 컬럼을 추가한다.
  - **CSV**: 후보 구분자 `,` `\t` `;` `|` 중 표본 줄(따옴표 인식 분할)의 필드 수가 가장 일정한 것(최빈값 비율 ≥ 90%, 필드 수 ≥ 2)을 선택한다. `has_header`는 첫 줄의 필드가 모두 비어있지 않고 숫자가 아니며 서로 다르고, 둘째 줄 이후에 숫자 필드가 있는 열이 하나라도 있을 때 true다. `names`는 헤더면 첫 줄 필드, 아니면 `"열1".."열N"`이다.
  - **그 외**: `Plain` (`names` 빈 목록). 빈 파일·섞인 파일·일반 로그도 `Plain`이다.
- 줄 분할 `split_fields(&str, &Columns) -> Vec<String>`: 반환 길이는 항상 `names.len()`이다(부족하면 `""` 채움, 넘치면 마지막 컬럼에 나머지를 구분자로 합침).
  - CSV는 RFC4180 따옴표(`""` 이스케이프)를 인식한다.
  - JSONL은 문자열은 값 그대로, 숫자/불/null은 JSON 표기 그대로, 객체/배열은 compact JSON으로 반환한다. `names`에 없는 키는 `_extra` 컬럼에 compact JSON 객체로 모으고, 파싱 실패 줄은 첫 컬럼에 원문을 넣고 나머지는 `""`다.
  - `Plain`이면 줄 전체 하나를 반환한다.
- 필드 필터: `FilterRule.field`에 컬럼 이름을 지정하면 그 규칙은 줄 전체가 아니라 `split_fields`로 분할한 해당 컬럼 값에만 매치한다. `FilterSet::new(spec, columns)`는 `columns`가 없거나 지정된 컬럼 이름이 없으면 `"규칙 N: 컬럼 'x' 를 찾을 수 없습니다"` 에러를 반환한다. `field`가 없는 규칙과 레벨 조건·include/exclude·AND/OR 의미는 그대로 유지된다. 필드 규칙이 하나도 없으면 줄 분할을 수행하지 않아 기존 경로와 동일한 비용을 유지한다. 필드 규칙의 `rule_highlights`는 생략한다.

### 3.7 여러 로그의 시간순 병합 (Phase 17)

열려 있는 탭 2~8개를 골라 타임스탬프 기준으로 섞은 **가상 파일**을 새 탭으로 연다. 실제 파일은 만들지 않으며, 병합 탭도 일반 탭처럼 가상 스크롤·검색·필터를 쓴다.

- **타임스탬프 추출** (`timestamp.rs`): 줄 앞 64바이트 안에서 첫 매치를 밀리초 단위 `i64`로 변환한다. 인식 형식: ISO-8601(`2026-11-12T07:15:42.134Z`, `+09:00`, 공백 구분, `,` 소수점, 소수 초 1~9자리), `2026/11/12 07:15:42`, syslog `Nov 12 07:15:42`(연도는 원본 파일 수정 시각의 연도), Apache `[12/Nov/2026:07:15:42 +0900]`, 유닉스 epoch 10자리(초)/13자리(밀리초). 시간대 표기가 없으면 로컬 시간대로 간주하며, 로컬 시간대 오프셋은 프론트(JS `Date#getTimezoneOffset`의 부호 반전값)가 `local_offset_minutes`로 넘긴다. 날짜→epoch 계산은 새 의존성 없이 직접 구현한다.
- **타임스탬프 상속**: 타임스탬프가 없는 줄(스택 트레이스 등)은 같은 원본의 바로 앞 줄의 유효 타임스탬프를 상속해 앞 줄과 붙어 다닌다. 원본 첫 줄들에 타임스탬프가 없으면 -∞(맨 앞)으로 본다.
- **병합 규칙**: 각 원본은 줄 순서대로만 소비한다(원본이 시간순이 아니어도 순서를 바꾸지 않는다 — k-way 병합의 "다음 줄" 비교만). 동일 타임스탬프는 원본 순번이 작은 쪽이 먼저 온다(안정 정렬). 원본이 각각 시간순이면 병합 결과도 비감소다.
- **희소 병합 인덱스** (`MergeIndex`): 병합 결과 K(=1024)줄마다 체크포인트 = 각 원본의 `(다음에 읽을 줄 번호, 상속용 마지막 유효 타임스탬프)`를 저장한다. 메모리는 (전체 줄 수/1024) × 원본 수 × 16바이트 수준. 임의 위치 읽기(`resolve`)는 가장 가까운 체크포인트부터 최대 1023줄의 k-way 병합을 재실행한다. 블록 첫 줄의 타임스탬프(`block_ts`)를 별도 보관해 `position_at_time`(시각 → 그 시각 이상 첫 병합 위치)을 이진 탐색+최대 K줄 재실행으로 계산한다.
- **구축**: `build()`는 백그라운드 스레드에서 실행하고 진행률(`MergeProgress`)+취소를 지원한다. 원본 인덱싱이 끝난 뒤 시작하며, 시작 시점의 확인된 줄 수·파일 길이를 스냅샷으로 쓴다(tail/이후 변경은 추적하지 않는 병합 시점 스냅샷). 각 원본은 순차 스트리밍으로 읽으며 줄 앞 64바이트만 보관한다(전체 데이터 1회 읽기). 구축 중에도 확정된 앞부분은 `resolve`로 읽을 수 있다(체크포인트 점진 공개).
- **역변환**: `position_of(source, line) -> Option<u64>`는 원본 줄 → 병합 위치, `map_positions(&[원본별 오름차순 줄 목록]) -> Vec<u64>`는 한 번의 병합 재실행으로 전체를 변환한다(k-way). `resolve_many`는 오름차순 병합 위치 목록을 같은 K 블록끼리 커서를 공유해 변환한다.
- **검색·필터 위임**: 병합 탭의 검색/필터는 각 원본에 기존 `search`/`filter_scan`을 실행한 뒤 원본 줄을 `map_positions`로 병합 위치로 변환해 `Vec<u64>`/`LineSet`을 채운다(원본별 결과는 이미 정렬). gz·원격 원본도 `Source` 추상화로 동일하게 동작한다.
- **수명**: `MergeIndex`는 원본 `Arc<LogFile>`을 보유하므로 원본 탭이 닫혀도 병합 탭이 살아 있고, 병합 탭을 닫으면 원본 참조가 해제된다.

### 3.8 원격 에이전트 (Phase 18)

SFTP 원격 파일은 인덱싱·검색·필터 시 파일 전체를 네트워크로 읽어야 한다. **에이전트**는 원격 서버에서 riffle-core로 그 작업을 직접 수행하고 결과(희소 체크포인트, 줄 번호, 구간)만 돌려준다. 화면에 보이는 줄 읽기는 기존 SFTP(RandomRead) 경로를 그대로 쓴다.

- **호스트별 opt-in, 기본 꺼짐.** 켠 호스트의 `sftp://` 파일을 열 때만 사용한다. 설정은 `settings.json`의 `agentHosts: Vec<String>`에 저장한다.
- **설치 위치**: 원격 `~/.cache/riffle/riffle-agent-<버전>-<arch>` (`<버전>`은 워크스페이스 버전과 동일 — `AGENT_VERSION = env!("CARGO_PKG_VERSION")`이고 `riffle-agent`의 hello 버전도 같은 값이다, `<arch>`는 `x86_64`|`aarch64`). 그 외 원격에는 아무것도 설치·수정하지 않는다. 바이너리는 `x86_64-unknown-linux-musl`/`aarch64-unknown-linux-musl` 정적 빌드를 릴리스 CI가 만들어 앱 리소스 `agents/`에 포함한다. 개발·테스트는 `RIFFLE_AGENT_DIR`로 디렉터리를 지정한다.
- **준비 절차**: `ssh <host> uname -sm`으로 아키텍처 확인(`Linux x86_64`/`Linux aarch64`만 지원) → 원격 경로에 같은 크기의 바이너리가 없으면 SFTP 업로드(임시 이름 `.tmp-<pid>`로 쓰기 → 권한 0755 → rename, 같은 크기면 생략) → `ssh -T -o BatchMode=yes … <host> "$HOME/.cache/riffle/<이름>" --stdio` 실행(홈 확장은 원격 셸에 맡김) → `hello`/`open` 핸드셰이크.
- **폴백**: 아키텍처 미지원, 바이너리 없음, 업로드/실행 실패, 프로토콜 오류 등 어느 단계든 실패하면 조용히 기존 SFTP 경로로 폴백한다(기능 손실 없음). 호출자가 `RemoteError::Agent`를 받아 폴백 여부를 판단한다.
- **인덱스 채택**: 에이전트 `index`의 `{ totalBytes, lineCount, checkpoints }`를 `LineIndex::from_checkpoints`로 완료 상태 인덱스로 재구성하고 `LogFile::adopt_index`로 채택한다. 로컬 인덱싱이 이미 진행됐거나, 미완료 인덱스이거나, 파일 크기가 다르면 `Error::IndexAdopt`로 거부된다.
- **에이전트 구현**: `crates/riffle-agent`(bin). 의존성은 `serde`(derive)·`serde_json`·`riffle-core`뿐이며 외부 네이티브 의존성이 없어 musl 정적 빌드가 가능하다. 동기 스레드 구조: stdin 읽기 스레드 + 작업(index/search/filter)별 스레드 + stdout 쓰기 `Mutex`(한 응답 = 완전한 JSON 한 줄, 줄 단위 flush). 한 세션은 파일 하나이며 작업은 동시에 여러 개 실행 가능하다. stdin EOF 시 진행 중 작업을 취소하고 스레드를 정리한다. `riffle-agent --version`은 `riffle-agent <버전> protocol 1`을 출력한다.
- **프로토콜 (줄 단위 JSON, `riffle-agent --stdio`)**: 요청 1줄 → 응답 여러 줄 스트리밍. FilterSpec은 IPC의 FilterSpecDto와 동일한 JSON이다.

```
→ {"id":1,"op":"hello"}                          ← {"id":1,"hello":{"version":"<버전>","protocol":1}}
→ {"id":2,"op":"open","path":"/var/log/app.log"} ← {"id":2,"opened":{"size":123,"gzip":false}}   (gzip이면 앱이 SFTP 경로로 폴백)
→ {"id":3,"op":"index"}                          ← {"id":3,"progress":{"indexedBytes":..,"lineCount":..}} … (200ms 간격)
                                                  ← {"id":3,"index":{"totalBytes":..,"lineCount":..,"checkpoints":[0,..]}}  (K=1024, LineIndex와 동일 의미)
→ {"id":4,"op":"search","query":{"query":"x","regex":false,"caseSensitive":true},"filter":null|FilterSpec}
                                                  ← {"id":4,"batch":[줄번호,…]}(≤10,000개) … ← {"id":4,"done":{"total":..,"capped":false}}
→ {"id":5,"op":"filter","spec":FilterSpec}        ← {"id":5,"ranges":[[s,e],…]}(≤10,000구간) … ← {"id":5,"done":{…}}
→ {"id":6,"op":"cancel","target":4}               (응답 없음, 대상 작업은 {"id":4,"cancelled":true}로 끝남)
오류: {"id":N,"error":"메시지"}  (알 수 없는 op·잘못된 JSON도 error 응답 후 세션은 계속 동작)
```

- **클라이언트**: `riffle_remote::AgentSession`이 자식 프로세스(ssh 또는 로컬)와 줄 단위 JSON을 주고받는다. `AgentEvent`(Progress/Batch/Ranges/Done/Cancelled/Error)로 스트리밍 결과를 전달하고, 테스트는 `AgentSession::spawn_local`(ssh 없이 로컬 바이너리 직접 실행)이나 `Transport::Command`로 검증한다.

### 3.9 테스트 필수 케이스

빈 파일, 개행 없는 마지막 줄, CRLF, 개행만 있는 파일, 16KB를 넘는 단일 줄, 잘못된 UTF-8 바이트, 청크 경계에 정확히 걸치는 줄, 청크 경계에 걸치는 검색 매치, 한글이 포함된 줄의 하이라이트 범위, K의 배수 번째 줄.

## 4. IPC 설계 (`src-tauri`)

모든 커맨드는 `Result<T, String>` 형태로 직렬화 가능한 에러를 반환한다.

| 커맨드               | 입력                                                                                                   | 출력                          | 비고                                                    |
| -------------------- | ------------------------------------------------------------------------------------------------------ | ----------------------------- | ------------------------------------------------------- |
| `open_file`          | `path`, `on_progress: Channel<IndexProgress>`                                                          | `FileInfo { id, path, size, compressed, remote, source, agent }` | 인덱싱을 백그라운드로 시작하고 즉시 반환. `path`는 canonical 경로. `sftp://호스트/경로`이면 원격(SFTP), `s3://버킷/키`이면 S3 객체로 열기 (Phase 12·13). `source`는 `"local" \| "sftp" \| "s3" \| "merge"`. `agent`는 에이전트 모드로 열렸을 때만 true (Phase 18) |
| `get_lines`          | `id`, `start`, `count`, `query?`, `spec?`, `with_fields?`                                              | `Vec<Line>` (+ 하이라이트)    | count 상한 1,000. spec은 규칙 하이라이트용. withFields=true이고 형식이 plain이 아니면 `Line.fields`에 컬럼 분할 값 (Phase 16) |
| `search`             | `id`, `query`, `filter_id?`, `on_result: Channel<SearchEvent>`                                          | `search_id`                   | 이전 검색 자동 취소. filter_id 지정 시 필터와 교집합     |
| `cancel_search`      | `search_id`                                                                                            | `()`                          |                                                         |
| `validate_filter`    | `id?`, `spec`                                                                                          | `()`                          | FilterSet 컴파일 가능 여부 (오류에 규칙 번호 포함). 필드 규칙은 id 파일의 컬럼으로 검증 (Phase 16) |
| `start_filter`       | `id`, `spec`, `on_event: Channel<SearchEvent>`                                                          | `filter_id`                   | 같은 파일의 이전 필터 자동 취소. 백그라운드 스캔         |
| `cancel_filter`      | `filter_id`                                                                                            | `()`                          |                                                         |
| `get_filter`         | `filter_id`                                                                                            | `FilterInfo`                  | 조건, 현재 줄 수, done/capped                            |
| `get_filter_lines`   | `id`, `filter_id`, `start_index`, `count`, `query?`, `spec?`, `with_fields?`                           | `Vec<Line>`                   | 필터 결과 목록의 [start_index, +count) 줄. count ≤ 1,000. withFields 시 `Line.fields` 포함 (Phase 16) |
| `get_filtered_lines` | `id`, `search_id`, `start_index`, `count`, `query?`, `spec?`, `with_fields?`                           | `Vec<Line>`                   | 검색 결과 목록의 [start_index, +count) 줄. count ≤ 1,000. withFields 시 `Line.fields` 포함 (Phase 16) |
| `filter_index_of`    | `filter_id`, `line`                                                                                    | `u64`                         | line 이상인 첫 보이는 줄의 index (스크롤 위치 유지)      |
| `filter_density`     | `filter_id`, `bins`, `total_lines`                                                                     | `Vec<u32>`                    | 스크롤바 마커용 밀도                                     |
| `list_filter_presets`| -                                                                                                      | `Vec<FilterPreset>`           | 이름순                                                  |
| `save_filter_preset` | `name`, `spec`                                                                                         | `()`                          | 같은 이름 덮어쓰기. 유효하지 않은 spec은 거부            |
| `delete_filter_preset`| `name`                                                                                                | `()`                          | 없으면 에러                                              |
| `copy_lines`         | `id`, `start`, `end`                                                                                   | `()`                          | Rust에서 직접 클립보드에 기록 (대량 복사 대비)            |
| `close_file`         | `id`                                                                                                   | `()`                          | 인덱싱/검색/필터 취소 및 자원 해제                       |
| `detect_columns`     | `id`                                                                                                   | `Columns`                     | 파일 형식 감지 결과 (`format: "plain"\|"csv"\|"jsonl"`, `delimiter?`, `hasHeader`, `names`). 파일별로 캐시하고 표본 읽기는 `spawn_blocking`에서 수행 (Phase 16) |
| `take_pending_open`  | -                                                                                                      | `string[]`                    | Finder 연결 대기 경로 목록 (콜드 스타트, 1회 수령)        |
| `save_session`       | `session: { paths, activeIndex }`                                                                      | `()`                          | 탭 세션을 session.json에 원자적 저장                      |
| `load_session`       | -                                                                                                      | `Session \| null`             | 탭 세션 로드. `restoreSession` 꺼짐/파일 없음/경로 전멸 시 null |
| `list_ssh_hosts`     | -                                                                                                      | `Vec<SshHost>`                 | `~/.ssh/config`의 Host 별칭 목록 (Phase 12)              |
| `list_remote_dir`    | `host`, `path`                                                                                         | `Vec<RemoteEntry>`             | 원격 디렉터리 목록. 이름순, 디렉터리 먼저 (Phase 12)      |
| `remote_home`        | `host`                                                                                                 | `string`                       | 원격 홈 디렉터리 절대 경로 (Phase 12)                     |
|| `get_agent_hosts`    | -                                                                                                      | `Vec<String>`                  | 에이전트를 켠 호스트 목록 (settings.json `agentHosts`) (Phase 18) |
|| `set_agent_enabled`  | `host`, `enabled`                                                                                      | `()`                           | 호스트별 에이전트 사용 on/off를 `agentHosts`에 저장 (Phase 18) |
| `list_aws_profiles`  | -                                                                                                      | `Vec<String>`                  | `~/.aws/config`(+`credentials`)의 프로필 이름 목록 (Phase 13). 이름만 읽고 키 값은 읽지 않음 |
| `s3_list_buckets`    | `profile?`                                                                                             | `Vec<String>`                  | ListBuckets. 권한 없으면 오류 (Phase 13)                  |
| `s3_list_prefix`     | `profile?`, `bucket`, `prefix`                                                                         | `Vec<S3Entry>`                 | 접두사를 `/` 구분자로 나열. 폴더 먼저, 이름순 (Phase 13)  |
| `s3_head`            | `profile?`, `uri`                                                                                      | `u64`                          | HeadObject로 객체 크기(바이트) 반환 (Phase 13)            |
| `set_s3_profile`     | `profile?`                                                                                             | `()`                           | 마지막 선택 프로필을 `settings.json`의 `s3Profile`에 저장 (Phase 13) |
| `get_s3_bookmarks`   | -                                                                                                      | `Vec<String>`                  | `app_data_dir/s3-bookmarks.json`에서 `"bucket"`/`"bucket/prefix/"` 목록 (Phase 13) |
| `set_s3_bookmarks`   | `items`                                                                                                | `()`                           | 즐겨찾기를 s3-bookmarks.json에 원자적 저장 (Phase 13)     |
| `get_bookmarks`      | `id`                                                                                                   | `BookmarksDto { items, stale }` | 경로/URI + 머리 지문으로 조회. 저장된 지문이 다르거나 크기가 줄었으면 `stale=true` (Phase 15) |
| `set_bookmarks`      | `id`, `items`                                                                                          | `()`                           | 줄 번호 오름차순 정렬·중복 제거 후 현재 지문/크기와 함께 저장 (stale 해소). 빈 목록이면 항목 삭제 (Phase 15) |
| `open_merge`         | `sourceIds`, `localOffsetMinutes`, `on_progress: Channel<IndexProgress>`                                | `FileInfo`                     | 열린 탭 2~8개를 타임스탬프 병합 뷰로 연다. `path`는 `"merge://<id>"`, `source`는 `"merge"`, `size`는 원본 크기 합. 병합 구축은 백그라운드 (Phase 17) |
| `merge_position_at_time` | `id`, `ts_ms`                                                                                     | `u64`                          | 유효 타임스탬프가 ts_ms 이상인 첫 병합 위치. 없으면 병합 줄 수 (Phase 17) |
| `merge_sources`      | `id`                                                                                                   | `Vec<MergeSource>`             | `{ index, path, lineCount }` 원본 목록 (Phase 17)       |

- `IndexProgress { indexed_bytes, total_bytes, line_count, done, notice? }`. `notice`는 에이전트 폴백 사유 등 한 번만 채워 보내는 안내 문구다 (Phase 18).
- (Phase 11) Finder "다음으로 열기"·다중 선택·드래그 앤 드롭으로 열린 파일은 `"open-files"` 이벤트(`string[]`, 순서 유지)로 프론트에 전달한다. 콜드 스타트처럼 프론트 준비 전에 도착한 경로는 Rust가 `pending_open`에 누적하고 `take_pending_open`으로 한 번에 수령한다. 파일 열기·드래그 앤 드롭·최근 파일·세션 복원은 모두 새 탭으로 열고, 이미 열린 같은 경로(canonical `FileInfo.path` 기준)면 해당 탭으로 전환한다.
- (Phase 11) 탭 세션은 `session.json`에 `{ paths, activeIndex }`로 저장한다. 프론트가 탭 목록/활성 탭 변경 시 500ms 디바운스로 `save_session`을 호출하고, 시작 시 `load_session`으로 복원한다(`restoreSession` 설정, 기본 켜짐). 존재하지 않는 경로는 로드 시 걸러낸다.
- `SearchEvent`는 `Progress { scanned_bytes, total_bytes, match_count }`, `Done { total, capped }`, `Cancelled`, `Error { message }` 중 하나. `start_filter`도 같은 이벤트 형식을 쓴다.
- `Line`에는 `highlights`(검색 매치, UTF-16)와 `rule_highlights`(규칙 매치 `[start, end, ruleIdx]`, UTF-16)가 있다. `fields`(컬럼 뷰 분할 값)는 `withFields=true`이고 파일 형식이 plain이 아닐 때만 채워진다 (Phase 16). 병합 뷰의 줄에는 `source`(원본 순번)와 `sourceLine`(원본 줄 번호)이 채워지고 `no`는 병합 위치다; 일반 파일은 둘 다 null이다 (Phase 17).
- `FilterSpec { rules: [{ pattern, regex, caseSensitive, kind: "include"|"exclude", enabled, color, field? }], combine: "and"|"or", levels: ["error"|"warn"|"info"|"debug"|"trace"] }`. `color`는 프론트 표시용이며 Rust는 저장만 한다. `field`는 컬럼 뷰 필드 필터용이며 지정된 컬럼 값에만 매치한다 (Phase 16).
- (Phase 16) 컬럼 뷰: `detect_columns`로 파일 형식(plain/csv/jsonl)·구분자·헤더 여부·컬럼명을 조회한다(파일별 캐시). `get_lines`/`get_filter_lines`/`get_filtered_lines`에 `withFields=true`를 넘기면 각 줄의 `fields`로 컬럼 값을 받는다. `start_filter`/`validate_filter`/`search`(filter_id 경유)는 파일의 감지된 컬럼으로 `FilterSet`을 컴파일하므로 `FilterRule.field`에 없는 컬럼명을 쓰면 `"규칙 N: 컬럼 'x' 를 찾을 수 없습니다"` 에러가 된다.
- (Phase 15) 북마크는 파일의 특정 줄에 붙이는 표시+메모+색이다. `BookmarkDto { line, note, color ("#rrggbb"), createdAt (unix 초) }`. 파일 식별은 열 때 기록된 경로/URI 문자열(로컬은 canonical, 원격은 `sftp://`·`s3://`)로 하고, `app_data_dir/bookmarks.json`에 `{ "<경로/URI>": { "fingerprint": "hex", "size": 크기, "updatedAt": 초, "items": [...] } }`로 원자적 저장한다(파일 항목 최대 500개, 마지막 수정 시각이 오래된 것부터 제거). 머리 지문은 `riffle_core::head_fingerprint`(파일 앞 64KB의 FNV-1a 64비트 해시, 로컬/원격/gz 해제본 모두 Source로 계산)와 저장 당시 크기를 함께 보관하며, 다시 열 때 지문이 다르거나 현재 크기가 저장된 크기보다 작으면 `stale=true`를 반환해(크기 증가는 로그 append로 정상이므로 stale로 보지 않음) "파일 내용이 바뀌어 북마크 위치가 다를 수 있다"를 프론트가 안내하게 한다(북마크는 지우지 않음). `set_bookmarks`는 현재 지문/크기로 갱신해 stale을 해소하고, 빈 목록이면 항목을 삭제한다. 지문 계산은 `spawn_blocking`에서 수행한다(원격은 네트워크 읽기 64KB).
- `TailEvent::Appended`에는 `filterCount`(활성 필터의 보이는 줄 수, 없으면 null)가 추가된다.
- (Phase 12) 원격(SFTP) 파일: `open_file`의 `path`가 `sftp://호스트/경로`이면 `SftpPool`(시스템 `ssh` + russh-sftp, 앱 전역 하나, 호스트별 참조 카운트)로 연다. `FileInfo.remote`는 true이고 `path`는 URI 그대로 보관해 최근 파일·세션 복원이 그대로 동작한다. `close_file`은 원격이면 `pool.release(host)`를 호출한다. 원격 파일의 tail은 notify 대신 1초 폴링 `refresh`를 사용한다. 연결 오류는 탭 에러 바로 표시하고 재연결은 RandomRead 구현체가 read 시 1회 시도한다.
- (Phase 12) `SshHost { alias, hostname?, user? }`, `RemoteEntry { name, isDir, size, mtime? }`(mtime은 unix 초). 모든 원격 호출은 `spawn_blocking`에서 수행한다. `list_ssh_hosts`는 `~/.ssh/config`(Include·~ 확장, 와일드카드/부정 별칭 제외)의 Host 목록을 반환한다.
- (Phase 18) 에이전트: `agentHosts`에 등록된 호스트의 `sftp://` 파일을 열면 `SftpPool::start_agent`로 세션을 시작하고, 인덱싱은 에이전트 `index`의 체크포인트를 `adopt_index`로 채택해 진행률 이벤트를 그대로 전달한다. `search`·`start_filter`도 에이전트에 위임하고, 위임 실패 시 누적 결과를 비우고 일반 SFTP/로컬 스캔으로 폴백한다. 세션 시작 실패(아키텍처 미지원, 번들에 해당 arch 바이너리 없음, 버전/프로토콜 불일치 등)면 `IndexProgressDto.notice`에 사유를 실어 보낸다. 에이전트 바이너리 디렉터리는 `RIFFLE_AGENT_DIR` 환경 변수 우선, 없으면 `resource_dir()/agents`다. `FileInfo.agent`는 세션이 실제로 열렸을 때만 true다.
- (Phase 13) S3 객체: `open_file`의 `path`가 `s3://버킷/키`이면 `S3Pool`(aws-sdk-s3, 앱 전역 하나, (프로필, 리전)별 클라이언트 캐시 + 버킷별 리전 캐시)로 연다. URI에는 프로필을 넣지 않고, 마지막 선택 프로필은 `settings.json`의 `s3Profile`에 저장한다(없으면 AWS 기본 자격 증명 체인). `FileInfo.remote`는 true, `source`는 `"s3"`이며 `path`는 URI 그대로 보관해 최근 파일·세션 복원이 그대로 동작한다. `close_file`은 별도 해제 없이 클라이언트를 풀에서 재사용한다. S3 객체는 불변이므로 `set_tail`은 `Err("S3 객체는 tail 을 지원하지 않습니다")`를 반환한다. RandomRead 구현은 `stat_len`을 HeadObject ContentLength로 얻고, 큰 요청은 8MB Range GET을 최대 16개 동시에, 작은 요청은 1MB 블록 LRU 캐시(최대 64MB)로 읽는다. 일시적 오류는 지수 백오프로 최대 3회 재시도한다. 읽기 전용 API(HeadObject, GetObject Range, ListObjectsV2, ListBuckets, GetBucketLocation/HeadBucket)만 호출한다.
- (Phase 13) `S3Entry { name, isDir, size, mtime? }`(mtime은 unix 초, 폴더는 null). `list_aws_profiles`는 `~/.aws/config`의 `[default]`/`[profile x]`와 `~/.aws/credentials`의 `[x]` 섹션 헤더에서 프로필 이름만 읽는다(정렬, 중복 제거, default 포함). 모든 S3 호출은 `spawn_blocking`에서 수행한다.
- (Phase 3 결정) 검색 결과 줄 번호 목록은 Rust에만 보관한다(최대 1,000만 건을 JS로 넘기지 않음). 프론트는 `search_nav`(다음/이전), `search_hit_at`, `search_density`(스크롤바 마커), `get_filtered_lines`(검색 필터 모드), `validate_query`로 필요한 만큼만 조회한다. 다중 필터의 보이는 줄도 `LineSet`으로 Rust에만 보관하고 `get_filter_lines`/`filter_index_of`/`filter_density`로 필요한 만큼만 조회한다.
- (Phase 17) 병합 뷰: `open_merge`가 만든 가상 파일 id에서도 `get_lines`/`search`/`start_filter`/`get_filter_lines`/`get_filtered_lines`/`filter_index_of`/`copy_lines`/`get_full_line`이 동작한다(내부에서 원본으로 위임). 검색 결과와 필터 `LineSet`은 병합 위치 기준으로 보관한다. `localOffsetMinutes`는 프론트가 `new Date().getTimezoneOffset()`의 부호를 반전해 넘긴다(로컬 시각 = UTC + offset). 병합 뷰는 tail·북마크·컬럼 뷰를 지원하지 않는다. 원본 탭을 닫아도 병합 탭은 `Arc`로 원본을 유지하고, 병합 탭을 닫으면 해제된다.
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
| Cmd+O               | 파일 열기 (새 탭)        |
| Cmd+Shift+O         | 원격 파일 열기 (Phase 12) |
| Cmd+W               | 탭 닫기                  |
| Cmd+Shift+W         | 창 닫기                  |
| Cmd+Shift+] / [     | 다음 / 이전 탭           |
| Cmd+1~9             | N번째 탭으로 이동        |
| Ctrl+Tab / Ctrl+Shift+Tab | 다음 / 이전 탭    |
| Cmd+F               | 검색창 포커스            |
| Enter / Shift+Enter | 다음 / 이전 결과         |
| Cmd+L               | 줄 번호로 이동           |
| ↑↓, PageUp/PageDown | 스크롤                   |
| Cmd+↑ / Cmd+↓       | 파일 처음 / 끝           |
| Cmd+T               | tail 모드 토글 (Phase 4) |

- (Phase 11) "창 닫기"는 `PredefinedMenuItem::close_window` 대신 커스텀 `MenuItem`(⌘⇧W)를 쓴다. predefined 항목은 macOS에서 단축키가 ⌘W로 고정되어 "탭 닫기"(⌘W)와 충돌하므로, 메뉴 이벤트를 받아 Rust에서 `window.close()`를 호출한다.

### 5.5 UI 구성

상단 검색바, 중앙 뷰포트(줄 번호 거터 + 본문 + 커스텀 스크롤바), 하단 상태바(파일 경로, 크기, 줄 수, 인덱싱 진행률, 검색 결과 n/N, 현재 줄). 시스템 라이트/다크 모드를 따른다.

- (Phase 12) 원격 파일 열기 대화상자(`Cmd+Shift+O`, 메뉴 `파일 > 원격 파일 열기…`, 빈 화면 버튼): `list_ssh_hosts` 호스트 목록 + 직접 입력 자동완성, `remote_home`으로 시작 경로 설정, `list_remote_dir` 디렉터리 탐색(이름/크기/수정 시각, 디렉터리 더블클릭 진입, 상위 이동, 파일 더블클릭/열기로 탭 오픈, 다중 선택), 로딩/에러(재시도) 표시, 최근 원격 위치(호스트+디렉터리, 최대 10개)를 localStorage에 저장한다. 원격 탭은 호스트 배지(예: `myhost:`)와 전체 URI 툴팁, 상태바에 `원격: host`를 표시하고, tail 버튼 툴팁에 `원격: 1초 간격 확인`을 표시한다.
- (Phase 13) 원격 파일 열기 대화상자 상단에 `[SSH | S3]` 세그먼트를 두고, S3 탭은 AWS 프로필 드롭다운(기본 체인 + `list_aws_profiles` 목록, 선택 시 `set_s3_profile`), 버킷 목록(`s3_list_buckets` 실패 시 "버킷 목록 권한이 없습니다 — 버킷 이름을 직접 입력하세요" + 입력창), 즐겨찾기(★ 토글, 목록 상단 표시), 접두사 브레드크럼, 폴더 진입/상위, 객체 크기·수정 시각, 다중 선택 열기를 제공한다. 1GB 이상 객체를 열 때는 객체 크기와 "전체 인덱싱·검색 시 AWS 외부로 약 N GB 전송(데이터 전송 요금 발생 가능)" 안내 확인 창을 띄우며("다시 묻지 않기"는 localStorage에 저장), S3 탭은 `s3:` + 버킷명 배지와 상태바 `S3: bucket`을 표시하고 tail 버튼을 비활성화한다.

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
- 3.8의 테스트 케이스 중 검색 외 항목 작성, 인덱싱 벤치마크 작성.
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
