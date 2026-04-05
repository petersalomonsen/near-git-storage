import { test, expect } from '@playwright/test';
import {
    createAccount,
    viewFunction,
} from './helpers/sandbox-rpc.js';

// The git-server already deploys the factory at factory.sandbox
// and the global contract at gitglobal.sandbox during startup.
const FACTORY_ID = 'factory.sandbox';
const USER_ID = 'alice.sandbox';

test.describe('Create Repository via Factory', () => {
    test.beforeAll(async () => {
        // Create test user account (git-server already started the sandbox)
        try {
            await createAccount(USER_ID, 'sandbox', 20);
        } catch (e) {
            if (!e.message.includes('AccountAlreadyExists')) throw e;
        }
    });

    test('create a new repo through the UI', async ({ page }) => {
        // Navigate with sandbox mode (uses near-api-js directly, no wallet needed)
        await page.goto(
            `/create-repo?factory=${FACTORY_ID}&sandbox=true&account=${USER_ID}&rpc=/near-rpc`
        );

        // Should auto-connect in sandbox mode
        await expect(page.locator('#wallet-info')).toContainText(USER_ID, {
            timeout: 30000,
        });

        // Fill in repo name
        await page.fill('#repo-name', 'my-test-repo');

        // Click create
        await page.click('#create-btn');

        // Wait for success
        await expect(page.locator('#result')).toBeVisible({ timeout: 60000 });
        await expect(page.locator('#result-details')).toContainText(
            `my-test-repo.${FACTORY_ID}`,
        );

        // Verify on-chain: the repo contract exists and owner is the user
        const owner = await viewFunction(
            `my-test-repo.${FACTORY_ID}`,
            'get_owner',
        );
        expect(owner).toBe(USER_ID);
    });

    test('repo created by factory has no access keys', async () => {
        const resp = await fetch('http://localhost:8080/near-rpc', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                jsonrpc: '2.0',
                id: 1,
                method: 'query',
                params: {
                    request_type: 'view_access_key_list',
                    finality: 'final',
                    account_id: `my-test-repo.${FACTORY_ID}`,
                },
            }),
        });
        const data = await resp.json();
        expect(data.result.keys).toHaveLength(0);
    });
});
