# Riffle (가칭)

macOS용 초대용량 로그 뷰어. 수 GB~수십 GB 텍스트 로그를 즉시 열고, 부드럽게 스크롤하고, 빠르게 검색한다.
상세 요구사항과 단계별 작업 계획은 `docs/SPEC.md`를 따른다. 작업 시작 전 반드시 읽는다.

## 기술 스택

- 앱 프레임워크: Tauri v2
- 코어: Rust (`crates/riffle-core`, Tauri 의존성 없는 순수 라이브러리)
- 프론트엔드: Vite + React + TypeScript + Tailwind CSS v4 + shadcn/ui
- 패키지 매니저: pnpm
- Next.js는 사용하지 않는다 (Tauri는 정적 SPA를 번들하므로 Vite가 적합).

## 아키텍처 원칙 (반드시 지킬 것)

1. **파일 데이터는 Rust에만 존재한다.** 프론트엔드는 화면에 보이는 구간(수백 줄)만 IPC로 요청한다. 파일 전체나 대량의 줄을 JS로 넘기지 않는다.
2. **`riffle-core`는 Tauri를 모른다.** 모든 파일 처리·인덱싱·검색 로직은 core에 두고, `src-tauri`는 얇은 IPC 래퍼 역할만 한다. core는 CLI 예제와 단위 테스트만으로 검증 가능해야 한다.
3. **무거운 작업은 백그라운드에서, 취소 가능하게.** 인덱싱·검색은 별도 스레드에서 실행하고, 진행 상황은 `tauri::ipc::Channel`로 스트리밍하며, 취소 플래그를 지원한다. UI 스레드를 막지 않는다.
4. **네이티브 스크롤에 의존하지 않는다.** 브라우저 요소 높이 상한 때문에 줄 수가 많으면 네이티브 스크롤바가 동작하지 않는다. 커스텀 가상 스크롤과 커스텀 스크롤바를 사용한다.

## 코드 스타일

- 들여쓰기: 공백 4칸 (탭 금지). Rust, TS, JSON, CSS 모두 동일.
- 중괄호: 여는 중괄호는 구문과 같은 줄, 닫는 중괄호는 블록 시작 위치와 같은 들여쓰기.
- Rust: `rustfmt` 기본 규칙 + `cargo clippy -- -D warnings` 통과. core의 에러 타입은 `thiserror`로 정의한다.
- TypeScript: strict 모드. Prettier `tabWidth: 4`. `any` 사용 금지.
- 가독성 최우선. 성능 최적화는 측정으로 병목이 확인된 경우에만 적용한다 (단, SPEC의 아키텍처 설계 자체는 예외).
- 불필요한 추상화, 트릭, 매크로 남용을 피한다.
- 코드 주석과 커밋 메시지: 한국어.
- 새 의존성(crate, npm 패키지) 추가 시 이유를 먼저 설명하고 승인받는다.
- 상태관리 라이브러리는 필요성이 명확해질 때까지 도입하지 않는다.

## 작업 방식

- `docs/SPEC.md`의 Phase 순서대로 진행한다. 한 번에 하나의 Phase만 작업한다.
- 각 Phase 시작 시 구현 계획(파일 목록, 주요 타입/함수, 순서)을 먼저 제시하고 승인 후 코드를 작성한다.
- 파일이 크거나(300줄 이상) 변경 범위가 넓으면 한 번에 전체를 수정하지 않는다. 함수/섹션 단위로 나눠 순차적으로 수정하고, 각 단계마다 diff를 보여준 뒤 다음 단계로 진행한다.
- Phase 완료 조건: 아래 검증 명령이 모두 통과하고, SPEC의 해당 Phase 완료 기준을 충족할 것.
- Phase가 끝나면 변경 요약을 보고하고 커밋한다 (Phase 단위 커밋).

## 검증 명령

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm typecheck
pnpm lint
pnpm tauri dev        # 수동 확인
```

## 테스트 데이터

- 대용량 테스트 파일은 `cargo run -p riffle-core --release --example gen_log -- <출력경로> <크기>` 로 생성한다.
- 생성된 테스트 파일은 `testdata/` 아래에 두고 git에 커밋하지 않는다 (`.gitignore` 등록).
