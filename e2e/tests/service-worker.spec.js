import { test, expect } from '@playwright/test';

async function getStorageUsage(contractId) {
    const resp = await fetch('http://localhost:8080/near-rpc', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
            jsonrpc: '2.0', id: 1,
            method: 'query',
            params: {
                request_type: 'view_account',
                finality: 'final',
                account_id: contractId,
            },
        }),
    });
    const data = await resp.json();
    return data.result.storage_usage;
}

test.describe('NEAR Git Storage - Service Worker variant', () => {
    test('publish a blog post and verify it appears', async ({ page }) => {
        const errors = [];
        page.on('console', msg => {
            if (msg.type() === 'error') errors.push(msg.text());
        });
        page.on('pageerror', err => errors.push(err.message));

        await page.goto('/sw-test');

        // Wait for the blog UI to be ready (posts container rendered after clone)
        await page.waitForSelector('#posts', { timeout: 120000 });
        await page.waitForSelector('#publish:not([disabled])', { timeout: 120000 });

        // Fill in the blog post form
        await page.fill('#title', 'Test Post');
        await page.fill('#body', 'Hello from Playwright');

        // Publish
        await page.click('#publish');

        // Wait for the post to appear
        await page.waitForSelector('#posts article', { timeout: 120000 });

        // Verify the post content is visible
        const postText = await page.locator('#posts article').first().textContent();
        expect(postText).toContain('Test Post');
        expect(postText).toContain('Hello from Playwright');

        if (errors.length) {
            console.log('Console errors:', errors);
        }
    });

    test('incremental push storage is small (delta compression)', async ({ page }) => {
        const errors = [];
        page.on('console', msg => {
            if (msg.type() === 'error') errors.push(msg.text());
        });

        await page.goto('/sw-test');
        await page.waitForSelector('#posts', { timeout: 120000 });
        await page.waitForSelector('#publish:not([disabled])', { timeout: 120000 });

        // First push — establishes baseline
        await page.fill('#title', 'First Post');
        await page.fill('#body', 'Initial content that is reasonably long to demonstrate delta compression savings when only a small edit is made later.');
        await page.click('#publish');
        await page.waitForSelector('#posts article', { timeout: 120000 });

        const storageAfterFirst = await getStorageUsage('repo.factory.sandbox');

        // Second push — small edit, should be delta-compressed
        await page.fill('#title', 'Second Post');
        await page.fill('#body', 'Just a tiny change.');
        await page.click('#publish');

        // Wait for the new post
        await page.waitForFunction(
            () => document.querySelectorAll('#posts article').length >= 2,
            { timeout: 120000 },
        );

        const storageAfterSecond = await getStorageUsage('repo.factory.sandbox');
        const storageIncrease = storageAfterSecond - storageAfterFirst;

        console.log(`Storage: first=${storageAfterFirst}, second=${storageAfterSecond}, increase=${storageIncrease}`);

        // The incremental push should use much less storage than the first push.
        // With delta compression, a small blog post edit should add < 3KB.
        expect(storageIncrease).toBeLessThan(3000);

        if (errors.length) {
            console.log('Console errors:', errors);
        }
    });
});
