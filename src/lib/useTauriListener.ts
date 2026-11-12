import { useEffect, useRef } from "react";

/**
 * Promise로 unlisten 함수를 돌려주는 Tauri 리스너를 안전하게 등록하는 훅.
 *
 * React StrictMode의 이중 마운트에서 첫 번째 리스너가 누수되지 않도록
 * cancelled 플래그로 등록/해제를 조정한다 (등록 Promise가 resolve되기 전에
 * cleanup되면 resolve 즉시 unlisten을 호출). 핸들러와 등록 함수는 ref로
 * 참조하므로 리스너는 마운트 시 1회만 등록되고 항상 최신 핸들러를 호출한다.
 */
export function useTauriListener<T>(
    register: (handler: (payload: T) => void) => Promise<() => void>,
    handler: (payload: T) => void,
): void {
    const registerRef = useRef(register);
    const handlerRef = useRef(handler);
    useEffect(() => {
        registerRef.current = register;
        handlerRef.current = handler;
    });

    useEffect(() => {
        let cancelled = false;
        let unlisten: (() => void) | undefined;
        registerRef
            .current((payload) => handlerRef.current(payload))
            .then((fn) => {
                // StrictMode 재마운트로 이미 cleanup된 경우 즉시 해제
                if (cancelled) {
                    fn();
                } else {
                    unlisten = fn;
                }
            })
            .catch((err) => {
                console.warn("Tauri 리스너 등록 실패:", err);
            });
        return () => {
            cancelled = true;
            unlisten?.();
        };
    }, []);
}
