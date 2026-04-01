import { defineConfig } from '@playwright/test';

export default defineConfig({
    testDir: './tests',
    timeout: 120000,
    workers: 1,
    use: {
        baseURL: 'http://localhost:8081',
    },
    webServer: [
        {
            command: 'cd .. && cargo run -p git-server',
            url: 'http://localhost:8080/near-info',
            reuseExistingServer: true,
            timeout: 60000,
        },
        {
            command: 'node serve.mjs',
            url: 'http://localhost:8081/ping',
            reuseExistingServer: true,
        },
    ],
});
