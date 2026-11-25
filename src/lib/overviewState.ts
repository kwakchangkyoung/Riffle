/**
 * 파일 개요 스캔의 프론트엔드 상태.
 * FileTab이 탭별로 보관하고 Timeline/Scrollbar/StatusBar가 읽는다.
 */
export type OverviewInfo =
    | { status: "running"; fraction: number }
    | {
          status: "done";
          /** 스캔 시점의 줄 수 (tail로 파일이 자라면 현재 줄 수보다 작아질 수 있음) */
          lineCount: number;
          posError: number[];
          posWarn: number[];
          hasTime: boolean;
          errorTotal: number;
          warnTotal: number;
      }
    | { status: "error"; message: string };
