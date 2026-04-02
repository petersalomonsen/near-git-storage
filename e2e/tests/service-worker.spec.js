import { test, expect } from '@playwright/test';

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
});
