import { test, expect } from '@playwright/test';

test.describe('NEAR Git Storage - HTTP Server variant', () => {
    test('push and clone via wasm-git through HTTP git server to NEAR sandbox', async ({ page }) => {
        const errors = [];
        page.on('console', msg => {
            if (msg.type() === 'error') errors.push(msg.text());
        });
        page.on('pageerror', err => errors.push(err.message));

        await page.goto('/');

        // Wait for demo to complete (push + delete + re-clone + verify)
        await page.waitForSelector('#demo-complete, #demo-error', { timeout: 120000 });

        if (errors.length) {
            console.log('Console errors:', errors);
        }

        const statusEl = page.locator('#demo-complete');
        await expect(statusEl).toBeVisible();

        // Verify the status text contains expected operations
        const text = await statusEl.textContent();
        expect(text).toContain('Pushed successfully');
        expect(text).toContain('Verified content: Hello from wasm-git via NEAR blockchain!');
        expect(text).toContain('All operations completed successfully!');
    });
});
