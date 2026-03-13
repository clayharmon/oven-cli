interface RunResult {
    runId: string;
    status: string;
    cost: string;
    prNumber: string;
}
export declare function run(): Promise<RunResult>;
export {};
