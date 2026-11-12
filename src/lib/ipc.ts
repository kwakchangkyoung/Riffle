import { Channel, invoke } from "@tauri-apps/api/core";

export interface FileInfo {
    id: number;
    path: string;
    size: number;
}

export interface IndexProgress {
    indexedBytes: number;
    totalBytes: number;
    lineCount: number;
    done: boolean;
}

export interface Line {
    no: number;
    text: string;
    byteLen: number;
    truncated: boolean;
}

/**
 * 파일을 열고 백그라운드 인덱싱 진행률을 수신합니다.
 */
export function openFile(
    path: string,
    onProgress: (p: IndexProgress) => void,
): Promise<FileInfo> {
    const channel = new Channel<IndexProgress>();
    channel.onmessage = onProgress;
    return invoke<FileInfo>("open_file", { path, onProgress: channel });
}

/**
 * 지정된 파일의 특정 범위 줄들을 가져옵니다.
 */
export function getLines(
    id: number,
    start: number,
    count: number,
): Promise<Line[]> {
    return invoke<Line[]>("get_lines", { id, start, count, query: null });
}

/**
 * 열린 파일을 닫고 자원을 해제합니다.
 */
export function closeFile(id: number): Promise<void> {
    return invoke<void>("close_file", { id });
}
