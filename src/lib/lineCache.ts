import {
    getFilteredLines,
    getLines,
    type Line,
    type QueryOptions,
} from "./ipc";

export const BLOCK_SIZE = 512;
export const MAX_BLOCKS = 64;

interface CachedBlock {
    blockIndex: number;
    lines: Line[];
    isIncomplete: boolean;
}

/**
 * 줄 단위 LRU 캐시 및 프리페치 관리 클래스.
 * 일반 모드와 필터 모드(검색 결과 매치 줄)를 모두 지원합니다.
 */
export class LineCache {
    private fileId: number | null = null;
    private searchId: number | null = null;
    private isFiltered = false;
    private query: QueryOptions | null = null;
    private lineCount = 0;
    private isDone = false;
    private blocks = new Map<number, CachedBlock>();
    private pending = new Set<number>();
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
        this.fileId = fileId;
        this.searchId = null;
        this.isFiltered = false;
        this.query = null;
        this.fetchSeq++;
        this.blocks.clear();
        this.pending.clear();
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
     * 검색어를 변경하고 캐시를 초기화한 뒤 가시 범위를 즉시 재요청합니다.
     */
    public setQuery(query: QueryOptions | null): void {
        this.query = query;
        this.fetchSeq++;
        this.blocks.clear();
        this.pending.clear();
        this.refetchLastRange();
        this.notify();
    }

    /**
     * 필터 모드 데이터 소스를 설정하고 가시 범위를 즉시 재요청합니다.
     */
    public setFilteredSource(
        fileId: number | null,
        searchId: number | null,
        query: QueryOptions | null,
    ): void {
        this.fileId = fileId;
        this.searchId = searchId;
        this.isFiltered = true;
        this.query = query;
        this.fetchSeq++;
        this.blocks.clear();
        this.pending.clear();
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
     * 현재 보이는 범위 및 앞뒤 1블록의 데이터를 프리페치 요청합니다.
     */
    public requestRange(startLine: number, endLine: number): void {
        this.lastRequestedRange = [startLine, endLine];

        if (this.fileId === null) {
            return;
        }
        if (this.isFiltered && this.searchId === null) {
            return;
        }

        const minBlock = Math.floor(Math.max(0, startLine) / BLOCK_SIZE);
        const maxBlock = Math.floor(Math.max(0, endLine) / BLOCK_SIZE);

        this.visibleMinBlock = minBlock;
        this.visibleMaxBlock = maxBlock;

        // 현재 보이는 블록 + 앞뒤 1블록 프리페치
        const targetMin = Math.max(0, minBlock - 1);
        const maxPossibleBlock =
            this.lineCount > 0
                ? Math.floor((this.lineCount - 1) / BLOCK_SIZE)
                : 0;
        const targetMax = Math.min(maxPossibleBlock, maxBlock + 1);

        for (let b = targetMin; b <= targetMax; b++) {
            const block = this.blocks.get(b);
            if (block && !block.isIncomplete) {
                continue;
            }
            if (this.pending.has(b)) {
                continue;
            }
            this.fetchBlock(b);
        }
    }

    private fetchBlock(blockIndex: number): void {
        const currentFileId = this.fileId;
        if (currentFileId === null) {
            return;
        }
        const currentSearchId = this.searchId;
        if (this.isFiltered && currentSearchId === null) {
            return;
        }

        this.pending.add(blockIndex);
        const start = blockIndex * BLOCK_SIZE;
        const seq = this.fetchSeq;

        const request =
            this.isFiltered && currentSearchId !== null
                ? getFilteredLines(
                      currentFileId,
                      currentSearchId,
                      start,
                      BLOCK_SIZE,
                      this.query,
                  )
                : getLines(currentFileId, start, BLOCK_SIZE, this.query);

        request
            .then((lines) => {
                this.pending.delete(blockIndex);

                // 요청 도중 쿼리나 파일이 변경되었으면 무시
                if (this.fetchSeq !== seq) {
                    return;
                }

                // 빠른 스크롤 중 화면에서 멀어진(현재 블록에서 ±4블록 밖) 결과는 버림
                if (
                    blockIndex < this.visibleMinBlock - 4 ||
                    blockIndex > this.visibleMaxBlock + 4
                ) {
                    return;
                }

                const isIncomplete = lines.length < BLOCK_SIZE && !this.isDone;

                this.blocks.set(blockIndex, {
                    blockIndex,
                    lines,
                    isIncomplete,
                });

                // LRU 용량(최대 64개) 초과 시 가장 오래된 블록 제거
                if (this.blocks.size > MAX_BLOCKS) {
                    const oldestKey = this.blocks.keys().next().value;
                    if (oldestKey !== undefined) {
                        this.blocks.delete(oldestKey);
                    }
                }

                this.notify();
            })
            .catch((err) => {
                this.pending.delete(blockIndex);
                console.error(`블록 ${blockIndex} 로드 실패:`, err);
            });
    }

    /**
     * 특정 줄이 속한 캐시 블록을 제거하고, 보이는 범위이면 즉시 재요청합니다.
     * tail 모드에서 개행 없이 끝나던 마지막 줄이 이어진 경우 내용이 바뀔 수 있어 사용합니다.
     * 필터 모드(결과 인덱스 기준)에서는 지원하지 않습니다.
     */
    public invalidateLine(lineNo: number): void {
        if (this.fileId === null || this.isFiltered) {
            return;
        }
        const blockIndex = Math.floor(lineNo / BLOCK_SIZE);
        const existed = this.blocks.delete(blockIndex);
        this.pending.delete(blockIndex);
        if (!existed) {
            return;
        }
        if (
            blockIndex >= this.visibleMinBlock - 1 &&
            blockIndex <= this.visibleMaxBlock + 1
        ) {
            this.fetchBlock(blockIndex);
        }
        this.notify();
    }

    /**
     * 현재 캐시된 블록 수를 반환합니다.
     */
    public get size(): number {
        return this.blocks.size;
    }
}
