import { getLines, type Line } from "./ipc";

export const BLOCK_SIZE = 512;
export const MAX_BLOCKS = 64;

interface CachedBlock {
    blockIndex: number;
    lines: Line[];
    isIncomplete: boolean;
}

/**
 * 줄 단위 LRU 캐시 및 프리페치 관리 클래스
 */
export class LineCache {
    private fileId: number | null = null;
    private lineCount = 0;
    private isDone = false;
    private blocks = new Map<number, CachedBlock>();
    private pending = new Set<number>();
    private visibleMinBlock = 0;
    private visibleMaxBlock = 0;
    private listeners = new Set<() => void>();

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
     * 새 파일 설정 시 기존 캐시와 요청 큐를 초기화합니다.
     * fileId가 null인 경우(파일 닫기/전환 시작 시)에만 줄 수와 완료 여부를 초기화합니다.
     */
    public setFile(fileId: number | null): void {
        this.fileId = fileId;
        this.blocks.clear();
        this.pending.clear();
        if (fileId === null) {
            this.lineCount = 0;
            this.isDone = false;
            this.visibleMinBlock = 0;
            this.visibleMaxBlock = 0;
        }
        this.notify();
    }

    /**
     * 인덱싱 진행 상황에 맞춰 줄 수와 완료 여부를 갱신합니다.
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
                    // 현재 보이는 범위 또는 인접 프리페치 영역에 속하면 재요청
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
        if (this.fileId === null) {
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

        this.pending.add(blockIndex);
        const start = blockIndex * BLOCK_SIZE;

        getLines(currentFileId, start, BLOCK_SIZE)
            .then((lines) => {
                this.pending.delete(blockIndex);

                // 응답 도착 시 파일 id가 달라졌으면 버림
                if (this.fileId !== currentFileId) {
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
     * 현재 캐시된 블록 수를 반환합니다.
     */
    public get size(): number {
        return this.blocks.size;
    }
}
