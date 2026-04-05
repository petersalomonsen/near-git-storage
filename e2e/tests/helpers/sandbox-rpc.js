/**
 * Sandbox RPC helpers for E2E tests.
 *
 * Signs and sends transactions to the local NEAR sandbox using the
 * well-known genesis private key from the near-sandbox crate.
 */
import { KeyPair } from '@near-js/crypto';
import { actionCreators, createTransaction, Signature, SignedTransaction, GlobalContractDeployMode } from '@near-js/transactions';
import { createHash } from 'crypto';
import bs58 from 'bs58';

const SANDBOX_RPC = 'http://localhost:8080/near-rpc';

// near-sandbox crate default genesis key
const GENESIS_PRIVATE_KEY =
    'ed25519:3tgdk2wPraJzT4nsTuf86UX41xgPNk3MHnq8epARMdBNs29AFEztAuaQ7iHddDfXG9F2RzV1XNQYgJyAyoW51UBB';
const GENESIS_KEY_PAIR = KeyPair.fromString(GENESIS_PRIVATE_KEY);

/** Call a view function on the sandbox */
export async function viewFunction(accountId, methodName, args = {}) {
    const resp = await fetch(SANDBOX_RPC, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
            jsonrpc: '2.0', id: 1,
            method: 'query',
            params: {
                request_type: 'call_function',
                finality: 'final',
                account_id: accountId,
                method_name: methodName,
                args_base64: Buffer.from(JSON.stringify(args)).toString('base64'),
            },
        }),
    });
    const data = await resp.json();
    if (data.error || !data.result?.result) {
        throw new Error(`View call failed: ${JSON.stringify(data.error || data)}`);
    }
    return JSON.parse(Buffer.from(data.result.result).toString());
}

/** Sign and send a transaction using the genesis key */
export async function signAndSend(signerId, receiverId, actions) {
    const keyResp = await fetch(SANDBOX_RPC, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
            jsonrpc: '2.0', id: 1,
            method: 'query',
            params: {
                request_type: 'view_access_key',
                finality: 'final',
                account_id: signerId,
                public_key: GENESIS_KEY_PAIR.getPublicKey().toString(),
            },
        }),
    });
    const keyData = await keyResp.json();
    if (!keyData.result) {
        throw new Error(`Failed to get access key for ${signerId}: ${JSON.stringify(keyData.error || keyData)}`);
    }
    const nonce = keyData.result.nonce + 1;
    const blockHash = bs58.decode(keyData.result.block_hash);

    const tx = createTransaction(
        signerId,
        GENESIS_KEY_PAIR.getPublicKey(),
        receiverId,
        nonce,
        actions,
        blockHash,
    );

    const serialized = tx.encode();
    const hash = createHash('sha256').update(serialized).digest();
    const signed = GENESIS_KEY_PAIR.sign(hash);
    const sig = new Signature({ keyType: 0, data: signed.signature });
    const signedTx = new SignedTransaction({ transaction: tx, signature: sig });

    const result = await fetch(SANDBOX_RPC, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
            jsonrpc: '2.0', id: 1,
            method: 'broadcast_tx_commit',
            params: [Buffer.from(signedTx.encode()).toString('base64')],
        }),
    });

    const data = await result.json();
    if (data.error) throw new Error(`Transaction failed: ${JSON.stringify(data.error)}`);
    if (data.result?.status?.Failure) {
        throw new Error(`Execution failed: ${JSON.stringify(data.result.status.Failure)}`);
    }
    return data.result;
}

/** Create an account on the sandbox funded by a parent account */
export async function createAccount(newAccountId, parentAccountId, amountNear = 10) {
    const deposit = BigInt(amountNear) * BigInt('1000000000000000000000000');
    await signAndSend(parentAccountId, newAccountId, [
        actionCreators.createAccount(),
        actionCreators.transfer(deposit),
        actionCreators.addKey(GENESIS_KEY_PAIR.getPublicKey(), actionCreators.fullAccessKey()),
    ]);
}

/** Deploy a contract to an account */
export async function deployContract(accountId, wasmBytes, initMethod, initArgs) {
    const actions = [actionCreators.deployContract(wasmBytes)];
    if (initMethod) {
        actions.push(actionCreators.functionCall(
            initMethod,
            Buffer.from(JSON.stringify(initArgs || {})),
            BigInt('100000000000000'),
            BigInt(0),
        ));
    }
    await signAndSend(accountId, accountId, actions);
}

/** Deploy global contract code tied to an account */
export async function deployGlobalContract(accountId, wasmBytes) {
    const deployMode = new GlobalContractDeployMode({ accountId });
    await signAndSend(accountId, accountId, [
        actionCreators.deployGlobalContract(wasmBytes, deployMode),
    ]);
}

/** Call a function on a contract */
export async function functionCall(signerId, receiverId, methodName, args, deposit = '0') {
    return signAndSend(signerId, receiverId, [
        actionCreators.functionCall(
            methodName,
            Buffer.from(JSON.stringify(args)),
            BigInt('300000000000000'),
            BigInt(deposit),
        ),
    ]);
}

export { GENESIS_KEY_PAIR, GENESIS_PRIVATE_KEY };
