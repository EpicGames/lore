// End-to-end test of the lore-drive SvelteKit frontend in a real Chromium.
//
// Prereqs (see HANDOFF.md):
//   - lore-drive running on :8080 inside a scratch workspace
//   - `npm run dev` (vite, proxies /api → :8080) on :5173
//   - system Chrome at /opt/google/chrome/chrome
//
// Run: node e2e.mjs
import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';

const BASE = process.env.E2E_BASE ?? 'http://localhost:5173';
const CHROME = process.env.E2E_CHROME ?? '/opt/google/chrome/chrome';

let failures = 0;
function ok(cond, label) {
	console.log(`${cond ? '  ✔' : '  ✘ FAIL'} ${label}`);
	if (!cond) failures++;
}

// Scratch files for uploads
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'lore-e2e-'));
const mkfile = (rel, content) => {
	const p = path.join(tmp, rel);
	fs.mkdirSync(path.dirname(p), { recursive: true });
	fs.writeFileSync(p, content);
	return p;
};
const fileA = mkfile('alpha.txt', 'AAAA'); // 4 B
const fileB = mkfile('beta.txt', 'BBBBBBBB'); // 8 B
// a folder to upload via the webkitdirectory picker
mkfile('bundle/one.txt', '11111');
mkfile('bundle/sub/two.txt', '222222');
const bundleDir = path.join(tmp, 'bundle');

const browser = await chromium.launch({
	executablePath: CHROME,
	args: ['--no-sandbox', '--disable-dev-shm-usage']
});
const ctx = await browser.newContext({ acceptDownloads: true });
const page = await ctx.newPage();
page.on('pageerror', (e) => console.log('  [pageerror]', e.message));

const card = (name) => page.locator('.card', { has: page.locator(`.name:text-is("${name}")`) });
const cardMeta = (name) => card(name).locator('.meta');
const cardAddr = (name) => card(name).locator('.addr');
async function openMenu(name) {
	await card(name).locator('.burger').click();
}
async function waitIdle() {
	// refresh() toggles the Loading hint; just settle the network
	await page.waitForLoadState('networkidle');
}

console.log('— 1. Initial load —');
await page.goto(BASE);
await waitIdle();
ok(await page.locator('.brand .mode').textContent() === 'drive', 'header shows mode "drive"');
ok(await page.locator('.hint').textContent().then((t) => t.includes('empty')), 'empty root hint shown');

console.log('— 2. Create folder —');
await page.getByRole('button', { name: '+ Create folder' }).click();
await page.locator('.modal input').fill('docs');
await page.getByRole('button', { name: 'Create', exact: true }).click();
await card('docs').waitFor();
ok(true, 'folder "docs" appears');

console.log('— 3. Upload files via button —');
const fc1 = page.waitForEvent('filechooser');
await page.getByRole('button', { name: '↑ Upload files' }).click();
await (await fc1).setFiles([fileA, fileB]);
await card('alpha.txt').waitFor();
ok(await cardMeta('alpha.txt').textContent().then((t) => t.includes('4 B')), 'alpha.txt shows 4 B');
ok(await cardMeta('beta.txt').textContent().then((t) => t.includes('8 B')), 'beta.txt shows 8 B');
const addrAlpha1 = (await cardAddr('alpha.txt').textContent()).trim();
ok(/^[0-9a-f]{64}-[0-9a-f]{32}$/.test(addrAlpha1), 'alpha.txt address is <b3-hash>-<file-id>');

console.log('— 4. Navigate into docs + upload a *folder* (webkitdirectory) —');
await card('docs').locator('.name.asdir').click();
await page.locator('.crumb:text-is("docs")').waitFor();
const fc2 = page.waitForEvent('filechooser');
await page.getByRole('button', { name: '↑ Upload folder' }).click();
await (await fc2).setFiles(bundleDir); // directory upload
await card('bundle').waitFor();
ok(true, 'uploaded folder "bundle" appears inside docs');
await card('bundle').locator('.name.asdir').click();
await card('one.txt').waitFor();
ok(await cardMeta('one.txt').textContent().then((t) => t.includes('5 B')), 'bundle/one.txt = 5 B');
await card('sub').locator('.name.asdir').click();
await card('two.txt').waitFor();
ok(await cardMeta('two.txt').textContent().then((t) => t.includes('6 B')), 'bundle/sub/two.txt = 6 B');

console.log('— 5. Breadcrumb navigation —');
await page.locator('.crumb').first().click(); // jump to root
await card('alpha.txt').waitFor();
ok((await page.locator('.crumb').count()) === 1, 'jumped back to root via breadcrumb');

console.log('— 6. Owner scenario: re-upload same nested path with different size —');
// 6a. upload folder1/file1 (4 B) by dropping synthetic files (fallback path:
//     webkitGetAsEntry is null on synthetic DataTransfer → getAsFile flat)
//     — so use the file picker with a crafted name instead: the picker
//     can't carry slashes, so upload via drag'n'drop into a folder after
//     navigating, matching how the modal path-matching must behave.
// Simplest faithful reproduction: navigate into docs, upload file named
// "one.txt" into docs/bundle conflict… — instead reproduce exactly:
// upload alpha.txt again at root with different content (12 B).
fs.writeFileSync(fileA, 'CCCCCCCCCCCC'); // 12 B now
const fc3 = page.waitForEvent('filechooser');
await page.getByRole('button', { name: '↑ Upload files' }).click();
await (await fc3).setFiles([fileA]);
await page.locator('.veil[aria-label="Upload conflicts"]').waitFor();
ok(true, '409 conflict modal opened');
const listed = await page.locator('.conflictlist li').allTextContents();
ok(listed.some((t) => t.includes('/alpha.txt')), 'modal lists "/alpha.txt"');

console.log('— 6a. Abort keeps old state —');
await page.getByRole('button', { name: 'Abort' }).click();
await waitIdle();
ok(await cardMeta('alpha.txt').textContent().then((t) => t.includes('4 B')), 'abort: alpha.txt still 4 B');

console.log('— 6b. Replace all updates BOTH content and metadata —');
const fc4 = page.waitForEvent('filechooser');
await page.getByRole('button', { name: '↑ Upload files' }).click();
await (await fc4).setFiles([fileA]);
await page.locator('.veil[aria-label="Upload conflicts"]').waitFor();
await page.getByRole('button', { name: 'Replace all' }).click();
await waitIdle();
await page.waitForFunction(
	() =>
		[...document.querySelectorAll('.card')].some(
			(c) => c.textContent.includes('alpha.txt') && c.textContent.includes('12 B')
		),
	{ timeout: 5000 }
).catch(() => {});
const metaAfter = await cardMeta('alpha.txt').textContent();
ok(metaAfter.includes('12 B'), `replace-all: alpha.txt now shows 12 B (owner's stale-size bug) [got: ${metaAfter.trim()}]`);
const addrAlpha2 = (await cardAddr('alpha.txt').textContent()).trim();
ok(addrAlpha2 !== addrAlpha1, 'replace-all: b3 address changed with content');

console.log('— 6c. Download reflects the new content —');
const dl1p = page.waitForEvent('download');
await openMenu('alpha.txt');
await card('alpha.txt').getByRole('menuitem', { name: 'Download' }).click();
const dl1 = await dl1p;
const dl1path = await dl1.path();
ok(fs.readFileSync(dl1path, 'utf8') === 'CCCCCCCCCCCC', 'downloaded alpha.txt = new 12-byte content');

console.log('— 7. Conflict path-matching inside a subfolder (replace SELECTED) —');
// Navigate into docs/bundle, then upload one.txt (conflicting, new content)
// and three.txt (non-conflicting) together; deselect nothing → then test
// selected-only by deselecting one conflict in a two-conflict upload.
await card('docs').locator('.name.asdir').click();
await card('bundle').locator('.name.asdir').click();
await card('one.txt').waitFor();
const oneNew = mkfile('one.txt', '1111111111'); // 10 B (was 5)
const three = mkfile('three.txt', '333');
const fc5 = page.waitForEvent('filechooser');
await page.getByRole('button', { name: '↑ Upload files' }).click();
await (await fc5).setFiles([oneNew, three]);
await page.locator('.veil[aria-label="Upload conflicts"]').waitFor();
const listed2 = await page.locator('.conflictlist li').allTextContents();
ok(
	listed2.some((t) => t.includes('/docs/bundle/one.txt')),
	'conflict path is ABSOLUTE virtual path "/docs/bundle/one.txt"'
);
// "Replace selected" with the (only) conflict selected: one.txt overwritten,
// three.txt (non-conflicting) must also be uploaded — this exercises
// conflictPathOf() matching after navigating into a subfolder.
await page.getByRole('button', { name: 'Replace selected' }).click();
await card('three.txt').waitFor();
await page.waitForFunction(
	() =>
		[...document.querySelectorAll('.card')].some(
			(c) => c.textContent.includes('one.txt') && c.textContent.includes('10 B')
		),
	{ timeout: 5000 }
).catch(() => {});
ok(await cardMeta('one.txt').textContent().then((t) => t.includes('10 B')), 'replace-selected: one.txt now 10 B');
ok(await cardMeta('three.txt').textContent().then((t) => t.includes('3 B')), 'non-conflicting three.txt uploaded too');

console.log('— 7b. Deselecting the only conflict disables "Replace selected" —');
const oneNewer = mkfile('one.txt', '1'.repeat(20)); // 20 B
const fc6 = page.waitForEvent('filechooser');
await page.getByRole('button', { name: '↑ Upload files' }).click();
await (await fc6).setFiles([oneNewer]);
await page.locator('.veil[aria-label="Upload conflicts"]').waitFor();
await page.locator('.conflictlist input[type="checkbox"]').first().uncheck();
ok(
	await page.getByRole('button', { name: 'Replace selected' }).isDisabled(),
	'"Replace selected" disabled with zero selection (by design)'
);
await page.getByRole('button', { name: 'Abort' }).click();
await waitIdle();
ok(await cardMeta('one.txt').textContent().then((t) => t.includes('10 B')), 'aborted: one.txt untouched (still 10 B)');

console.log('— 8. Drag-and-drop upload (synthetic DataTransfer, file fallback) —');
await page.locator('.crumb').first().click();
await card('alpha.txt').waitFor();
await page.evaluate(async () => {
	const dt = new DataTransfer();
	dt.items.add(new File(['DROPPED'], 'dropped.txt', { type: 'text/plain' }));
	const app = document.querySelector('.app');
	app.dispatchEvent(new DragEvent('dragenter', { bubbles: true, dataTransfer: dt }));
	app.dispatchEvent(new DragEvent('drop', { bubbles: true, cancelable: true, dataTransfer: dt }));
});
await card('dropped.txt').waitFor();
ok(await cardMeta('dropped.txt').textContent().then((t) => t.includes('7 B')), 'drag-and-drop upload works (getAsFile fallback)');

console.log('— 9. Rename —');
await openMenu('dropped.txt');
await card('dropped.txt').getByRole('menuitem', { name: 'Rename' }).click();
const ren = page.locator('input.rename');
await ren.fill('renamed.txt');
await ren.press('Enter');
await card('renamed.txt').waitFor();
ok(true, 'rename dropped.txt → renamed.txt');
const addrRen = (await cardAddr('renamed.txt').textContent()).trim();
ok(addrRen.split('-')[1].length === 32, 'renamed file keeps a file-id (address intact)');

console.log('— 10. Folder download as ZIP —');
const dl2p = page.waitForEvent('download');
await openMenu('docs');
await card('docs').getByRole('menuitem', { name: 'Download as ZIP' }).click();
const dl2 = await dl2p;
const zpath = await dl2.path();
const zbytes = fs.readFileSync(zpath);
ok(zbytes.length > 0 && zbytes[0] === 0x50 && zbytes[1] === 0x4b, 'folder download is a ZIP (PK magic)');

console.log('— 11. Delete (confirm dialog) —');
page.once('dialog', (d) => d.accept());
await openMenu('renamed.txt');
await card('renamed.txt').getByRole('menuitem', { name: 'Delete' }).click();
await card('renamed.txt').waitFor({ state: 'detached' });
ok(true, 'renamed.txt deleted after confirm');
// declining the dialog keeps the file
page.once('dialog', (d) => d.dismiss());
await openMenu('beta.txt');
await card('beta.txt').getByRole('menuitem', { name: 'Delete' }).click();
await page.waitForTimeout(500);
ok(await card('beta.txt').isVisible(), 'dismissing confirm keeps beta.txt');

console.log('— 12. .lorekeep hidden —');
await page.getByRole('button', { name: '+ Create folder' }).click();
await page.locator('.modal input').fill('emptyd');
await page.getByRole('button', { name: 'Create', exact: true }).click();
await card('emptyd').waitFor();
await card('emptyd').locator('.name.asdir').click();
await page.locator('.hint:has-text("empty")').waitFor();
// In drive mode staging records the bare directory, so mkdir needs no
// `.lorekeep` placeholder. Plant one via the API and confirm the UI
// keeps hiding it while the backend lists it.
const raw = await page.evaluate(async () => {
	const root = await fetch('/api/v1/tree').then((r) => r.json());
	const emptyd = root.children.find((c) => c.name === 'emptyd');
	const form = new FormData();
	form.append('file', new File([''], '.lorekeep'), '.lorekeep');
	await fetch(`/api/v1/upload?parent_id=${emptyd.node_id}`, { method: 'POST', body: form });
	const t = await fetch(`/api/v1/tree?node_id=${emptyd.node_id}`).then((r) => r.json());
	return t.children.map((c) => c.name);
});
ok(raw.includes('.lorekeep'), 'backend lists the planted .lorekeep');
// re-enter the folder to force a fresh listing through the UI
await page.locator('.crumb').first().click();
await card('emptyd').waitFor();
await card('emptyd').locator('.name.asdir').click();
await page.locator('.hint:has-text("empty")').waitFor();
ok((await page.locator('.card').count()) === 0, 'UI hides .lorekeep (folder shown empty)');

console.log('— 13. Revision change-tag visible in header —');
const revText = await page.locator('.workspace').textContent();
ok(/[0-9a-f]{8}/.test(revText), 'header shows a revision change-tag');

await browser.close();
console.log(failures === 0 ? '\nALL E2E CHECKS PASSED' : `\n${failures} E2E CHECK(S) FAILED`);
process.exit(failures === 0 ? 0 : 1);
