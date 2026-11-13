import {
    getFilteredLines,
    getFilterLines,
    getLines,
    type FilterSpec,
    type Line,
    type QueryOptions,
} from "./ipc";

export const BLOCK_SIZE = 512;
export const MAX_BLOCKS = 64;

/** 동시에 진행할 수 있는 블록 요청 수 상한 (빠른 스크롤/드래그 시 IPC 폭주 방지) */
const MAX_INFLIGHT = 6;
/** 블록 요청 실패 시 재시도 횟수 (최초 요청 포함 최대 3회) */
const MAX_ATTEMPTS = 2;
/** 재시도 기본 지연 시간. 시도 횟수만큼 배수가 붙는다 */
const RETRY_DELAY_MS = 200;

interface CachedBlock {
    blockIndex: number;
    lines: Line[];
    isIncomplete: boolean;
    /** 블록을 가져온 시점의 fetchSeq. 현재 seq와 다르면 stale(이전 쿼리 결과) */
    seq: number;
}

/** 줄 데이터를 가져오는 소스 종류 */
type CacheSource =
    | { kind: "normal" }
    | { kind: "search"; searchId: number }
    | { kind: "filter"; filterId: number };

/**
 * 줄 단위 LRU 캐시 및 프리페치 관리 클래스.
 * 일반 모드, 필터 모드(검색 결과 매치 줄), 다중 필터 모드(필터 통과 줄)를 지원합니다.
 */
export class LineCache {
    private fileId: number | null = null;
    private source: CacheSource = { kind: "normal" };
    private query: QueryOptions | null = null;
    /** 규칙 하이라이트 계산용 필터 스펙 (get_*의 spec 인자) */
    private ruleSpec: FilterSpec | null = null;
    /** 컬럼 뷰: get_*의 with_fields 인자 (true이면 각 줄의 fields를 함께 요청) */
    private withFields = false;
    private lineCount = 0;
    private isDone = false;
    /** true면 새 블록 요청을 보류한다 (캐시된 줄 표시는 유지) */
    private suspended = false;
    private blocks = new Map<number, CachedBlock>();
    // blockIndex -> 요청 토큰. 토큰이 바뀌면 진행 중이던 이전 요청 결과는 폐기한다
    private pending = new Map<number, number>();
    private requestToken = 0;
    private attempts = new Map<number, number>();
    private failedBlocks = new Set<number>();
    private dragging = false;
    // 요청 상한 때문에 미뤄둔 블록이 있으면 true. 요청 완료 시 재개에 사용
    private deferredFetch = false;
    private visibleMinBlock = 0;
    private visibleMaxBlock = 0;
    private listeners = new Set<() => void>();
    private fetchSeq = 0;
    private lastRequestedRange: [number, number] | null = null;

    /**
     * 캐시 상태 변경 리스너를 등록합니다.
     */
    public subscribe(listener: () => void): () => void {
        this.listeners.add(listener);
        return () => {
            this.listeners.delete(listener);
        };
    }

    private notify(): void {
        for (const listener of this.listeners) {
            listener();
        }
    }

    /**
     * 마지막으로 요청된 가시 범위를 즉시 다시 요청합니다.
     */
    private refetchLastRange(): void {
        const range = this.lastRequestedRange ?? [0, 50];
        this.requestRange(range[0], range[1]);
    }

    /**
     * 새 파일 설정 시 기존 캐시와 요청 큐를 초기화합니다.
     */
    public setFile(fileId: number | null): void {
        // 이미 분리된 상태에서 다시 분리 요청이 오면 아무것도 하지 않는다
        if (fileId === null && this.fileId === null) {
            return;
        }
        this.fileId = fileId;
        this.source = { kind: "normal" };
        this.query = null;
        this.ruleSpec = null;
        this.suspended = false;
        this.fetchSeq++;
        this.blocks.clear();
        this.pending.clear();
        this.attempts.clear();
        this.failedBlocks.clear();
        if (fileId === null) {
            this.lineCount = 0;
            this.isDone = false;
            this.visibleMinBlock = 0;
            this.visibleMaxBlock = 0;
            this.lastRequestedRange = null;
        } else {
            this.refetchLastRange();
        }
        this.notify();
    }

    /**
     * 검색어를 변경합니다. 캐시된 블록은 즉시 비우지 않고 stale로만 표시하여
     * 화면의 기존 텍스트가 유지되게 하고, 응답이 도착한 블록부터 교체합니다.
     */
    public setQuery(query: QueryOptions | null): void {
        if (this.query === query) {
            return;
        }
        this.query = query;
        this.fetchSeq++;
        this.attempts.clear();
        this.failedBlocks.clear();
        this.refetchLastRange();
        this.notify();
    }

    /**
     * 컬럼 분할 값(fields) 요청 여부를 변경합니다.
     * 캐시된 블록은 stale로 표시하고 보이는 범위를 재요청합니다.
     */
    public setWithFields(withFields: boolean): void {
        if (this.withFields === withFields) {
            return;
        }
        this.withFields = withFields;
        this.fetchSeq++;
        this.attempts.clear();
        this.failedBlocks.clear();
        this.refetchLastRange();
        this.notify();
    }

    /**
     * 규칙 하이라이트용 필터 스펙을 변경합니다.
     * 하이라이트 범위가 바뀌므로 캐시된 블록은 stale로 표시하고 보이는 범위를 재요청합니다.
     */
    public setRuleSpec(spec: FilterSpec | null): void {
        if (this.ruleSpec === spec) {
            return;
        }
        this.ruleSpec = spec;
        this.fetchSeq++;
        this.attempts.clear();
        this.failedBlocks.clear();
        this.refetchLastRange();
        this.notify();
    }

    /**
     * 블록 요청을 일시정지/재개합니다.
     * 정지 중에도 캐시된 줄은 계속 표시되지만 새 요청은 보내지 않습니다.
     */
    public setSuspended(suspended: boolean): void {
        if (this.suspended === suspended) {
            return;
        }
        this.suspended = suspended;
        if (!suspended) {
            this.refetchLastRange();
        }
    }

    /**
     * 필터 모드(검색 결과) 데이터 소스를 설정하고 가시 범위를 즉시 재요청합니다.
     * 필터 결과 인덱스 기준의 줄 매핑이 바뀌므로 캐시는 전부 비웁니다.
     */
    public setFilteredSource(
        fileId: number | null,
        searchId: number | null,
        query: QueryOptions | null,
    ): void {
        this.fileId = fileId;
        this.source =
            searchId === null
                ? { kind: "normal" }
                : { kind: "search", searchId };
        this.query = query;
        this.fetchSeq++;
        this.blocks.clear();
        this.pending.clear();
        this.attempts.clear();
        this.failedBlocks.clear();
        this.refetchLastRange();
        this.notify();
    }

    /**
     * 다중 필터 모드 데이터 소스를 설정하고 가시 범위를 재요청합니다.
     * preserveBlocks=true이면 기존 블록을 지우지 않고 stale로만 표시하여
     * 새 필터 결과가 도착하는 동안 이전 줄들이 화면에 남게 합니다.
     */
    public setFilterSource(
        fileId: number,
        filterId: number,
        query: QueryOptions | null,
        spec: FilterSpec,
        preserveBlocks = false,
    ): void {
        const fileChanged = fileId !== this.fileId;
        this.fileId = fileId;
        this.source = { kind: "filter", filterId };
        this.query = query;
        this.ruleSpec = spec;
        this.suspended = false;
        this.fetchSeq++;
        this.pending.clear();
        this.attempts.clear();
        this.failedBlocks.clear();
        if (fileChanged || !preserveBlocks) {
            this.blocks.clear();
        }
        this.refetchLastRange();
        this.notify();
    }

    /**
     * 캐시된 블록을 모두 비우고 가시 범위를 즉시 재요청합니다.
     */
    public clear(): void {
        this.fetchSeq++;
        this.blocks.clear();
        this.pending.clear();
        this.attempts.clear();
        this.failedBlocks.clear();
        this.refetchLastRange();
        this.notify();
    }

    /**
     * 인덱싱 또는 검색 진행 상황에 맞춰 줄 수와 완료 여부를 갱신합니다.
     */
    public setLineCount(lineCount: number, isDone = false): void {
        this.lineCount = lineCount;
        this.isDone = isDone;

        let needNotify = false;
        // 불완전 블록 재요청 확인
        for (const [blockIndex, block] of this.blocks.entries()) {
            if (block.isIncomplete) {
                const blockStart = blockIndex * BLOCK_SIZE;
                if (lineCount > blockStart + block.lines.length) {
                    if (
                        blockIndex >= this.visibleMinBlock - 1 &&
                        blockIndex <= this.visibleMaxBlock + 1
                    ) {
                        this.fetchBlock(blockIndex);
                        needNotify = true;
                    }
                } else if (isDone) {
                    block.isIncomplete = false;
                }
            }
        }

        if (needNotify) {
            this.notify();
        }
    }

    /**
     * 특정 줄 번호에 해당하는 Line 데이터를 가져옵니다.
     * 캐시에 없으면 undefined를 반환합니다.
     */
    public getLine(lineNo: number): Line | undefined {
        const blockIndex = Math.floor(lineNo / BLOCK_SIZE);
        const block = this.blocks.get(blockIndex);
        if (!block) {
            return undefined;
        }

        // LRU 갱신: 참조된 블록을 Map 끝으로 재삽입
        this.blocks.delete(blockIndex);
        this.blocks.set(blockIndex, block);

        const offset = lineNo - blockIndex * BLOCK_SIZE;
        return block.lines[offset];
    }

    /**
     * 캐시에 로드된 줄을 인덱스 순으로 최대 `limit`개 반환합니다.
     * (컬럼 뷰 기본 폭 표본용 — 새 요청을 발생시키지 않습니다)
     */
    public sampleLines(limit: number): Line[] {
        const out: Line[] = [];
        const keys = [...this.blocks.keys()].sort((a, b) => a - b);
        for (const k of keys) {
            const block = this.blocks.get(k);
            if (block === undefined) {
                continue;
            }
            for (const line of block.lines) {
                out.push(line);
                if (out.length >= limit) {
                    return out;
                }
            }
        }
        return out;
    }

    /**
     * 해당 줄이 속한 블록의 요청이 최종 실패했는지 확인합니다.
     */
    public isLineFailed(lineNo: number): boolean {
        return this.failedBlocks.has(Math.floor(lineNo / BLOCK_SIZE));
    }

    /**
     * 스크롤바 드래그 상태를 설정합니다.
     * 드래그 중에는 프리페치를 생략하고, 종료 시 미뤄둔 요청을 재개합니다.
     */
    public setDragging(dragging: boolean): void {
        if (this.dragging === dragging) {
            return;
        }
        this.dragging = dragging;
        if (!dragging) {
            this.refetchLastRange();
        }
    }

    /**
     * 현재 보이는 범위 및 앞뒤 1블록의 데이터를 프리페치 요청합니다.
     * 진행 중 요청 수가 상한에 도달하면 나머지는 요청 완료 시 이어서 처리됩니다.
     */
    public requestRange(startLine: number, endLine: number): void {
        this.lastRequestedRange = [startLine, endLine];

        if (this.fileId === null || this.suspended) {
            return;
        }

        const minBlock = Math.floor(Math.max(0, startLine) / BLOCK_SIZE);
        const maxBlock = Math.floor(Math.max(0, endLine) / BLOCK_SIZE);

        this.visibleMinBlock = minBlock;
        this.visibleMaxBlock = maxBlock;

        const maxPossibleBlock =
            this.lineCount > 0
                ? Math.floor((this.lineCount - 1) / BLOCK_SIZE)
                : 0;
        const targetMin = Math.max(0, minBlock - 1);
        const targetMax = Math.min(maxPossibleBlock, maxBlock + 1);

        // 보이는 블록부터 요청 (진행 중 요청 수 상한까지)
        for (let b = minBlock; b <= maxBlock; b++) {
            if (this.pending.size >= MAX_INFLIGHT) {
                this.deferredFetch = true;
                break;
            }
            this.fetchBlockIfNeeded(b);
        }

        // 프리페치(앞뒤 1블록): 스크롤바 드래그 중에는 생략
        if (this.dragging) {
            return;
        }
        for (let b = targetMin; b <= targetMax; b++) {
            if (b >= minBlock && b <= maxBlock) {
                continue;
            }
            if (this.pending.size >= MAX_INFLIGHT) {
                this.deferredFetch = true;
                break;
            }
            this.fetchBlockIfNeeded(b);
        }
    }

    /**
     * 블록이 최신 상태가 아니면 요청합니다.
     * 실패 확정 블록은 다음 세대(쿼리/파일 변경)까지 재요청하지 않습니다.
     */
    private fetchBlockIfNeeded(blockIndex: number): void {
        if (this.pending.has(blockIndex) || this.failedBlocks.has(blockIndex)) {
            return;
        }
        const block = this.blocks.get(blockIndex);
        if (block && !block.isIncomplete && block.seq === this.fetchSeq) {
            return;
        }
        this.fetchBlock(blockIndex);
    }

    private fetchBlock(blockIndex: number): void {
        const currentFileId = this.fileId;
        if (currentFileId === null || this.suspended) {
            return;
        }
        if (this.pending.has(blockIndex)) {
            return;
        }

        this.failedBlocks.delete(blockIndex);
        const token = ++this.requestToken;
        this.pending.set(blockIndex, token);
        const seq = this.fetchSeq;
        const attempt = this.attempts.get(blockIndex) ?? 0;
        const start = blockIndex * BLOCK_SIZE;

        const request =
            this.source.kind === "search"
                ? getFilteredLines(
                      currentFileId,
                      this.source.searchId,
                      start,
                      BLOCK_SIZE,
                      this.query,
                      this.ruleSpec,
                      this.withFields,
                  )
                : this.source.kind === "filter"
                  ? getFilterLines(
                        currentFileId,
                        this.source.filterId,
                        start,
                        BLOCK_SIZE,
                        this.query,
                        this.ruleSpec,
                        this.withFields,
                    )
                  : getLines(
                        currentFileId,
                        start,
                        BLOCK_SIZE,
                        this.query,
                        this.ruleSpec,
                        this.withFields,
                    );

        request
            .then((lines) => {
                // 이후 재요청/무효화로 토큰이 바뀐 경우 이전 결과는 폐기
                if (this.pending.get(blockIndex) !== token) {
                    return;
                }
                this.pending.delete(blockIndex);
                this.attempts.delete(blockIndex);

                if (this.fetchSeq !== seq) {
                    // 요청 중 쿼리/파일 세대가 바뀜: 결과를 버리고 최신 범위를 재요청
                    this.refetchLastRange();
                    return;
                }

                // 빠른 스크롤 중 화면에서 멀어진(현재 블록에서 ±4블록 밖) 결과는 버림
                const isFar =
                    blockIndex < this.visibleMinBlock - 4 ||
                    blockIndex > this.visibleMaxBlock + 4;
                if (!isFar) {
                    const isIncomplete =
                        lines.length < BLOCK_SIZE && !this.isDone;

                    this.blocks.set(blockIndex, {
                        blockIndex,
                        lines,
                        isIncomplete,
                        seq,
                    });

                    // LRU 용량(최대 64개) 초과 시 가장 오래된 블록 제거
                    if (this.blocks.size > MAX_BLOCKS) {
                        const oldestKey = this.blocks.keys().next().value;
                        if (oldestKey !== undefined) {
                            this.blocks.delete(oldestKey);
                        }
                    }

                    this.notify();
                }
                // 상한 때문에 미뤄둔 요청이 있으면 이어서 처리
                if (this.deferredFetch) {
                    this.deferredFetch = false;
                    this.refetchLastRange();
                }
            })
            .catch((err) => {
                if (this.pending.get(blockIndex) !== token) {
                    return;
                }
                this.pending.delete(blockIndex);
                console.error(`블록 ${blockIndex} 로드 실패:`, err);

                // 요청 슬롯이 비었으므로 미뤄둔 요청을 이어서 처리
                if (this.deferredFetch) {
                    this.deferredFetch = false;
                    this.refetchLastRange();
                }

                // 짧은 지연 후 제한적으로 재시도한다
                if (attempt < MAX_ATTEMPTS) {
                    this.attempts.set(blockIndex, attempt + 1);
                    setTimeout(
                        () => {
                            if (this.fetchSeq !== seq) {
                                this.attempts.delete(blockIndex);
                                return;
                            }
                            this.fetchBlock(blockIndex);
                        },
                        RETRY_DELAY_MS * (attempt + 1),
                    );
                    return;
                }

                // 최종 실패: 해당 블록은 "불러오기 실패" 플레이스홀더로 표시
                this.attempts.delete(blockIndex);
                this.failedBlocks.add(blockIndex);
                this.notify();
            });
    }

    /**
     * 특정 줄이 속한 캐시 블록을 제거하고, 보이는 범위이면 즉시 재요청합니다.
     * tail 모드에서 개행 없이 끝나던 마지막 줄이 이어진 경우 내용이 바뀔 수 있어 사용합니다.
     * 검색 결과 모드(결과 인덱스 기준)에서는 지원하지 않습니다.
     * 다중 필터 모드에서는 인자가 필터 인덱스입니다.
     */
    public invalidateLine(lineNo: number): void {
        if (this.fileId === null || this.source.kind === "search") {
            return;
        }
        const blockIndex = Math.floor(lineNo / BLOCK_SIZE);
        const hadBlock = this.blocks.delete(blockIndex);
        // 진행 중 요청은 토큰 불일치로 결과가 폐기되도록 pending에서 제거
        this.pending.delete(blockIndex);
        this.attempts.delete(blockIndex);
        this.failedBlocks.delete(blockIndex);
        const inVisibleRange =
            blockIndex >= this.visibleMinBlock - 1 &&
            blockIndex <= this.visibleMaxBlock + 1;
        if (inVisibleRange) {
            this.fetchBlock(blockIndex);
        }
        // 캐시된 블록이 실제로 사라진 경우에만 리렌더가 필요하다
        if (hadBlock) {
            this.notify();
        }
    }

    /**
     * 현재 캐시된 블록 수를 반환합니다.
     */
    public get size(): number {
        return this.blocks.size;
    }
}
