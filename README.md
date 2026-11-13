<p align="center">
    <img src="docs/images/icon.png" width="128" alt="Riffle 아이콘">
</p>

<h1 align="center">Riffle</h1>

<p align="center">수 GB~수십 GB 로그를 즉시 열고, 부드럽게 스크롤하고, 빠르게 검색하는 macOS 로그 뷰어</p>

![검색 하이라이트](docs/images/search.png)

텍스트 에디터나 Console 앱이 멈추는 크기의 로그 파일을 위한 읽기 전용 뷰어입니다. 파일 데이터는 Rust 코어에만 두고 화면에 보이는 수백 줄만 UI로 보내기 때문에, 파일 크기와 상관없이 메모리를 적게 쓰고 반응이 빠릅니다.

## 주요 기능

- **즉시 열기:** 전체 인덱싱이 끝나기 전에 첫 화면을 띄우고, 인덱싱 중에도 스크롤할 수 있습니다.
- **빠른 검색:** 리터럴/정규식, 대소문자 구분을 지원합니다. 결과는 찾는 즉시 표시되고, 한글 검색어도 정확한 위치에 하이라이트됩니다.
- **다중 필터:** 로그 레벨 칩과 포함/제외 규칙(리터럴·정규식, AND/OR 결합)으로 보이는 줄을 제한합니다. 규칙별 색상 하이라이트와 프리셋 저장이 가능합니다.
- **검색 결과만:** 검색에 걸린 줄만 모아 봅니다. 왼쪽 거터에는 원래 줄 번호가 그대로 표시됩니다. 필터가 켜져 있으면 필터를 통과한 줄 안에서만 검색합니다.
- **스크롤바 마커:** 검색 결과가 파일 어디에 몰려 있는지 스크롤바에 밀도로 표시합니다.
- **tail 모드:** 파일 끝에 추가되는 줄을 실시간으로 따라갑니다. 위로 스크롤하면 자동 스크롤이 멈추고, 로그 로테이션(rename, truncate)이 감지되면 자동으로 다시 엽니다.
- **압축 로그(.gz) 지원:** gzip 파일을 열면 백그라운드에서 해제하면서 바로 내용을 볼 수 있습니다. 해제된 내용은 임시 공간을 사용하며 파일을 닫으면 해제됩니다. tail 모드는 지원하지 않습니다.
- **로그 레벨 색상:** ERROR, WARN, INFO, DEBUG/TRACE 토큰을 색으로 구분합니다.
- **줄 단위 선택과 대량 복사:** 줄 번호를 클릭하고 Shift+클릭으로 범위를 넓힌 뒤 Cmd+C를 누르면, 10만 줄 이상도 Rust에서 직접 클립보드로 복사합니다.
- **긴 줄 전체 보기:** 16KB가 넘어 잘린 줄은 오른쪽 사이드 패널에서 원문 전체를 볼 수 있습니다.
- **기타:** 줄 번호로 이동, 폰트 크기 조절, 최근 파일 목록, macOS 네이티브 메뉴, `.log`/`.txt` 파일 연결, 시스템 라이트/다크 모드.

| 필터 모드                            | 긴 줄 전체 보기                              |
| ------------------------------------ | -------------------------------------------- |
| ![필터 모드](docs/images/filter.png) | ![긴 줄 전체 보기](docs/images/fullline.png) |

## 성능

Apple M1(메모리 16GB) 내장 SSD(순차 읽기 약 3.5GB/s)에서 10GB(6,533만 줄) 로그로 측정한 값입니다. 측정 전에 다른 10GB 파일을 읽어 대상 파일을 페이지 캐시에서 밀어낸 상태(콜드)입니다.

| 항목                                        | 결과                                | 목표  |
| ------------------------------------------- | ----------------------------------- | ----- |
| 파일 열기 → 첫 화면(20줄)                   | 1.3ms (인덱싱 완료를 기다리지 않음) | 300ms |
| 전체 줄 인덱싱                              | 3.3초                               | 10초  |
| 리터럴 검색 (544만 건 매치)                 | 3.1~3.3초, 첫 결과 2ms 이내         | 10초  |
| 정규식 검색 (파일 전체 스캔)                | 3.0초                               | —     |
| 10만 줄(16MB) 범위 읽기                     | 약 17ms                             | —     |
| 코어 메모리 (CLI 측정, OS 페이지 캐시 제외) | 인덱싱 38MB, 검색 56MB              | 200MB |

인덱싱과 검색은 디스크 순차 읽기 속도가 상한입니다(같은 파일의 `dd` 읽기 3.07초). 외장 USB SSD(약 670MB/s)에서는 인덱싱 15.4초, 검색 15.3초였습니다.

## 설치

### 요구 사항

- macOS 11 (Big Sur) 이상 (Apple Silicon, Intel 모두 지원)

### 릴리스에서 다운로드 (권장)

[Releases 페이지](https://github.com/kwakchangkyoung/Riffle/releases/latest)에서 Mac에 맞는 DMG를 내려받습니다.

- Apple Silicon(M1 이후): `Riffle_<버전>_aarch64.dmg`
- Intel Mac: `Riffle_<버전>_x64.dmg`

### DMG로 설치

1. 내려받은 DMG를 열고 `Riffle.app`을 `Applications` 폴더로 드래그합니다.
2. 이 앱은 아직 Apple 코드 서명과 공증을 받지 않았습니다. 다른 곳에서 내려받은 DMG라서 처음 실행할 때 "확인되지 않은 개발자" 경고가 나오면, Finder에서 `Riffle.app`을 **우클릭 → 열기**로 한 번 실행하세요. 또는 아래 명령으로 격리 속성을 지울 수 있습니다.

    ```bash
    xattr -dr com.apple.quarantine /Applications/Riffle.app
    ```

### 소스에서 빌드

필요한 도구: [Rust](https://rustup.rs) (stable), Node.js 20 이상, [pnpm](https://pnpm.io), Xcode Command Line Tools

```bash
git clone https://github.com/kwakchangkyoung/Riffle.git
cd Riffle
pnpm install
pnpm tauri build
```

빌드 결과물:

- 앱: `target/release/bundle/macos/Riffle.app`
- 설치 이미지: `target/release/bundle/dmg/Riffle_0.1.0_aarch64.dmg`

```bash
# 앱 설치 후 LaunchServices에 등록 ("다음으로 열기" 목록에 바로 나타나게)
ditto target/release/bundle/macos/Riffle.app /Applications/Riffle.app
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f /Applications/Riffle.app
```

## 사용법

### 파일 열기

- **Cmd+O** 또는 상단의 **파일 열기** 버튼
- 로그 파일을 창으로 **드래그 앤 드롭**
- Finder에서 `.log`/`.txt`/`.gz` 파일 **우클릭 → 다음으로 열기 → Riffle**
- 빈 화면의 **최근 파일** 목록, 또는 메뉴 **파일 → 최근 파일**

파일을 열면 첫 화면이 바로 표시됩니다. 하단 상태바에서 인덱싱 진행률과 지금까지 확인된 줄 수를 볼 수 있습니다.

### 검색

1. **Cmd+F**로 검색창에 포커스를 옮기고 검색어를 입력합니다. 입력을 멈추면 바로 검색이 시작됩니다.
2. `.*`는 정규식, `Aa`는 대소문자 구분 토글입니다. 정규식이 잘못되면 입력창 아래에 오류가 표시되고 검색은 시작하지 않습니다.
3. **Enter / Shift+Enter**(또는 ▲▼ 버튼)로 다음/이전 결과로 이동합니다. 결과 위치는 `n / N`으로 표시됩니다.
4. **검색 결과만** 버튼을 켜면 검색 결과 줄만 보입니다. 이 보기에서 줄을 더블클릭하면 필터를 끄고 그 줄로 이동합니다. 다중 필터가 켜져 있으면 검색은 필터를 통과한 줄 안에서만 수행됩니다.

### 다중 필터

헤더 아래 필터 바에서 보이는 줄을 제한합니다. 필터는 파일을 바꿔도 유지되며 앱을 다시 시작하면 초기화됩니다.

- **레벨 칩:** ERROR / WARN / INFO / DEBUG / TRACE를 클릭해 켜고 끕니다. 모두 켜진 상태는 조건 없음과 같습니다. Option+클릭은 해당 레벨만 남깁니다.
- **규칙:** `+ 규칙` 입력에 패턴을 넣고 Enter로 추가합니다. `.*`는 정규식, `Aa`는 대소문자 구분, 포함/제외 세그먼트로 규칙 종류를 고릅니다. 규칙 칩을 클릭하면 켜기/끄기, `×`로 삭제, 더블클릭으로 편집합니다.
- **결합 방식:** 켜진 포함 규칙이 2개 이상이면 AND/OR 토글이 나타납니다. 제외 규칙은 켜진 규칙 중 하나라도 매치되면 그 줄을 숨깁니다.
- **색상:** 규칙마다 팔레트에서 색을 고를 수 있고, 매치된 부분이 그 색으로 반투명하게 하이라이트됩니다(검색 하이라이트가 더 위에 표시됩니다).
- **프리셋:** 필터 바 오른쪽 **프리셋** 메뉴에서 현재 필터를 이름으로 저장하고, 목록에서 불러오거나 삭제할 수 있습니다.
- 계산은 백그라운드에서 진행되며, 끝날 때까지 이전 결과가 그대로 표시됩니다. 상태바에 `필터: n / 전체줄`과 진행률이 표시됩니다.

### tail 모드

**Cmd+T** 또는 **Tail** 버튼으로 켭니다. 화면이 맨 아래에 있으면 새 줄을 자동으로 따라갑니다. 위로 스크롤하면 자동 스크롤이 일시 정지되고 **새 줄 N개 ↓** 버튼이 나타납니다. 이 버튼을 누르면 다시 맨 아래로 이동합니다.

### 선택과 복사

- 화면에 보이는 텍스트는 마우스로 드래그해 일반 복사할 수 있습니다.
- 큰 범위는 **줄 번호를 클릭**해 선택하고, **Shift+클릭**으로 범위를 넓힌 뒤 **Cmd+C**를 누릅니다. 상태바에 `n줄 복사됨 (크기)`가 표시됩니다. 한 번에 최대 100MB까지 복사할 수 있습니다.
- **Esc**로 선택을 해제합니다.

### 긴 줄 보기

16KB가 넘는 줄은 화면에서 잘려 보이고, 오른쪽 끝에 **전체 보기** 배지가 붙습니다. 배지를 클릭하거나 줄을 더블클릭하면 사이드 패널에서 원문 전체를 볼 수 있습니다(최대 10MB).

### 단축키

| 키                         | 동작                                 |
| -------------------------- | ------------------------------------ |
| Cmd+O                      | 파일 열기                            |
| Cmd+F                      | 검색창 포커스                        |
| Cmd+Shift+F                | 필터 규칙 입력창 포커스              |
| Enter / Shift+Enter        | 다음 / 이전 검색 결과                |
| Cmd+L                      | 줄 번호로 이동                       |
| ↑ ↓ / PageUp PageDown      | 한 줄 / 한 화면 스크롤               |
| Cmd+↑ / Cmd+↓ (Home / End) | 파일 처음 / 끝                       |
| Cmd+T                      | tail 모드 토글                       |
| Cmd+C                      | 선택한 줄 복사                       |
| Cmd+= / Cmd+- / Cmd+0      | 글자 크게 / 작게 / 기본 크기         |
| Esc                        | 검색창 해제, 줄 선택 해제, 패널 닫기 |

## 자동 업데이트

앱이 GitHub Releases의 `latest.json`을 확인해 새 버전을 알려줍니다.

- **시작 시 자동 확인:** 앱 실행 후 3초 뒤 백그라운드에서 확인합니다. 새 버전이 있을 때만 다이얼로그가 표시되고, 최신 버전이거나 네트워크 오류면 아무 표시 없이 지나갑니다.
- **수동 확인:** 메뉴 **Riffle → 업데이트 확인…** 을 누르면 결과(최신 버전 / 새 버전 / 오류)가 항상 표시됩니다.
- **끄기:** 메뉴 **Riffle → 시작 시 업데이트 자동 확인** 의 체크를 해제합니다.
- 다이얼로그에서 **업데이트**를 누르면 다운로드와 설치가 진행됩니다. 완료 후 **지금 재시작**을 누르면 새 버전으로 다시 시작합니다.

참고: 자동 업데이트는 이 기능이 포함된 릴리스부터 동작합니다. v0.2.0 이하 버전을 쓰고 있다면 한 번은 DMG를 수동으로 설치해 새 버전으로 올린 뒤부터 자동 업데이트를 사용할 수 있습니다.

## 릴리스 방법 (메인테이너용)

버전 태그를 push하면 GitHub Actions(`.github/workflows/release.yml`)가 Apple Silicon과 Intel용 DMG를 빌드해 릴리스에 첨부합니다. 자동 업데이트용 업데이터 산출물(`.app.tar.gz` + `.sig`, `latest.json`)도 함께 만들어집니다.

업데이터 산출물을 서명하려면 저장소 Secrets에 서명 키 2개가 필요합니다.

- `TAURI_SIGNING_PRIVATE_KEY`: `pnpm tauri signer generate`로 만든 개인키 내용
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: 개인키 비밀번호

공개키는 `src-tauri/tauri.conf.json`의 `plugins.updater.pubkey`에 넣습니다(현재 자리표시자). 키를 새로 만들면 함께 갱신해야 합니다. Secrets가 없으면 릴리스 워크플로가 실패합니다.

1. `src-tauri/tauri.conf.json`, `package.json`, `src-tauri/Cargo.toml`의 `version`을 새 버전으로 올리고 커밋합니다.
2. 같은 버전의 태그를 만들어 push합니다. 태그와 `tauri.conf.json` 버전이 다르면 워크플로가 실패합니다.

    ```bash
    git tag v0.2.0
    git push origin main v0.2.0
    ```

3. 진행 상황은 저장소의 **Actions** 탭에서 확인합니다. 빌드가 끝나면 **Releases**에 DMG가 올라갑니다.

## 개발

```bash
pnpm install
pnpm tauri dev          # 개발 모드 실행

# 검증 명령
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm typecheck
pnpm lint
```

테스트용 대용량 로그는 생성기로 만듭니다(`testdata/`는 git에서 제외됩니다).

```bash
cargo run -p riffle-core --release --example gen_log -- testdata/big_10g.log 10G
# tail 모드 테스트: 초당 5,000줄 추가 + 20초마다 로테이션
cargo run -p riffle-core --release --example append_log -- testdata/tail.log --rate 5000 --rotate-every 20
```

### 구조

```
crates/riffle-core/   순수 Rust 코어: 파일 읽기(read_at), 희소 줄 인덱스, 줄 읽기, 검색, tail(refresh)
src-tauri/            Tauri 앱: IPC 커맨드, 파일 감시, 네이티브 메뉴, 최근 파일
src/                  React + TypeScript UI: 가상 스크롤, 커스텀 스크롤바, 검색바
docs/SPEC.md          요구사항과 설계 문서
```

자세한 설계는 [`docs/SPEC.md`](docs/SPEC.md)를 참고하세요.
