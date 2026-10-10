// Real React/MUI rendering; only the Tauri boundary is simulated. No installs/process shutdown.
const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs'), path = require('node:path'), vm = require('node:vm');
const ts = require('typescript');
const {JSDOM} = require('jsdom');
const React = require('react');
const {act} = React;
const {createRoot} = require('react-dom/client');
const dom = new JSDOM('<!doctype html><html><body></body></html>', {url: 'http://localhost/', pretendToBeVisual: true});
for (const name of ['window', 'document', 'localStorage', 'HTMLElement', 'Element', 'Node', 'DocumentFragment', 'ShadowRoot', 'MutationObserver', 'MouseEvent']) global[name] = dom.window[name];
global.getComputedStyle = dom.window.getComputedStyle.bind(dom.window);
global.IS_REACT_ACT_ENVIRONMENT = true;
window.matchMedia = () => ({matches: false, addEventListener() {}, removeEventListener() {}});
const baseApp = {revision: 1, name: 'ok-nte', icon: '', website: null, current_version: null, available_versions: [], running: false,
    installed: false, current_profile: 'China', profiles: [{name: 'China'}], update_source: 'git', installation: null,
    update_state: 'idle', update_target_version: null, update_error: null, source_operation_state: 'idle',
    mirrorchyan: {resource_id: 'ok-nte'}, auto_start: false, update_method: 'MANUAL_UPDATE', operation: null};

async function fixture(overrides = {}, savedLogs = [], hasCdk = true) {
    localStorage.clear();
    localStorage.setItem('pyappifyConsoleLogs', JSON.stringify({'ok-nte': savedLogs}));
    const listeners = new Map(), calls = [], pending = [], cache = new Map(), failures = new Map();
    let app = {...baseApp, ...overrides}, sequence = 0;
    const emit = (name, payload) => {for (const handler of listeners.get(name) ?? []) handler({payload});};
    const backend = {
        async listen(name, handler) {if (!listeners.has(name)) listeners.set(name, new Set()); listeners.get(name).add(handler); return () => listeners.get(name).delete(handler);},
        invoke(command, args) {
            calls.push({command, args});
            if (failures.has(command)) return Promise.reject({kind: 'msg', message: failures.get(command)});
            if (command === 'load_app') {emit('app', app); return Promise.resolve(app);}
            if (command === 'mirrorchyan_has_cdk') return Promise.resolve(hasCdk);
            if (command === 'get_config_payload') return Promise.resolve([]);
            if (command === 'get_update_notes') return Promise.resolve(['Release notes']);
            if (command === 'get_app_icon') return Promise.resolve(null);
            if (command === 'setup_app' || command === 'update_to_version') {
                const source = app.update_source === 'git' ? 'git' : 'mirror';
                const installedSource = app.installation?.source ?? 'git';
                const kind = source + '_' + (command === 'setup_app' && app.installed && installedSource === app.update_source ? 'configure'
                    : command === 'update_to_version' && app.installed && installedSource === app.update_source ? 'update' : 'install');
                const operation = {id: args.operationId, app_name: app.name, sequence: ++sequence, kind, status: 'running', can_cancel: true,
                    target_version: args.version ?? null, previous_version: app.current_version, profile: args.profileName ?? null, error: null};
                app = {...app, operation};
                emit('app-operation', operation);
                return new Promise((resolve, reject) => pending.push({operation, resolve, reject}));
            }
            if (command === 'cancel_app_operation') {
                const operation = app.operation;
                assert.equal(args.operationId, operation.id);
                const next = {...operation, status: 'cancelling', sequence: ++sequence};
                app = {...app, operation: next}; emit('app-operation', next);
                return Promise.resolve(true);
            }
            return Promise.resolve();
        },
    };
    const mocks = {'@tauri-apps/api/core': {invoke: backend.invoke}, '@tauri-apps/api/event': {listen: backend.listen},
        '@tauri-apps/api/app': {getVersion: async () => '1.0.0'}, '@tauri-apps/api/window': {getCurrentWindow: () => ({setTitle: async () => {}})},
        '@tauri-apps/plugin-opener': {openUrl: async () => {}}};
    function load(filename) {
        filename = path.resolve(filename);
        if (cache.has(filename)) return cache.get(filename).exports;
        const module = {exports: {}}; cache.set(filename, module);
        const code = ts.transpileModule(fs.readFileSync(filename, 'utf8'), {compilerOptions: {module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2022}}).outputText;
        const localRequire = request => {
            if (mocks[request]) return mocks[request];
            if (request.endsWith('.css')) return {};
            if (request === '@mui/icons-material') return Object.fromEntries(['Build','Cached','Delete','KeyboardArrowRight','OpenInNew','PlayArrow','Settings','StopCircle'].map(name => [name, require('@mui/icons-material/' + name).default]));
            if (!request.startsWith('.')) return require(request);
            const absolute = path.resolve(path.dirname(filename), request);
            const target = [absolute, absolute + '.ts', absolute + '.tsx'].find(file => fs.existsSync(file));
            return load(target);
        };
        new vm.Script('(function(require,module,exports){' + code + '\n})', {filename}).runInThisContext()(localRequire, module, module.exports);
        return module.exports;
    }
    const App = load('src/App.tsx').default;
    await require('i18next').changeLanguage('en');
    const container = document.createElement('div'); document.body.append(container);
    const root = createRoot(container);
    await act(async () => {root.render(React.createElement(React.StrictMode, null, React.createElement(App)));});
    const buttons = () => [...container.querySelectorAll('button')];
    const button = text => buttons().find(element => element.textContent.trim() === text);
    const click = async text => {
        const element = button(text); assert.ok(element, 'Missing button: ' + text); assert.equal(element.disabled, false, 'Disabled button: ' + text);
        await act(async () => {element.dispatchEvent(new MouseEvent('click', {bubbles: true}));});
    };
    return {container, calls, button, click, load, fail: (command, message) => failures.set(command, message), emit: async (name, payload) => act(async () => emit(name, payload)),
        operation: () => app.operation,
        async publishApp(change) {app = {...app, ...change, revision: app.revision + 1}; await act(async () => emit('app', app));},
        async complete(status, error = null) {
            const task = pending.shift(); assert.ok(task);
            const operation = {...app.operation, status, can_cancel: false, sequence: ++sequence, error};
            app = {...app, operation};
            await act(async () => {
                emit('app-operation', operation);
                if (status === 'succeeded') task.resolve();
                else task.reject({kind: status === 'cancelled' ? 'cancelled' : 'msg', message: error ?? 'Operation cancelled by user'});
            });
        },
        async dispose() {await act(async () => root.unmount()); container.remove();},
    };
}

for (const action of ['Install', 'Confirm Upgrade']) {
    for (const cdk of ['missing', 'unreadable']) test(`Mirror ${action} with ${cdk} CDK opens Settings before starting a task`, async () => {
        const f = await fixture({update_source: 'mirrorchyan', ...(action === 'Confirm Upgrade' ? {
            installed: true, current_version: 'v1.4.8', available_versions: ['v1.4.8', 'v1.4.9'],
            installation: {source: 'mirrorchyan', version: 'v1.4.8'},
        } : {})}, [], false);
        try {
            if (cdk === 'unreadable') f.fail('mirrorchyan_has_cdk', 'Unreadable CDK');
            await f.click(action);
            assert.equal(f.container.querySelector('h1')?.textContent, 'Settings');
            assert.ok(f.container.querySelector('input[type="password"]'));
            assert.ok(f.container.textContent.includes('Configure a MirrorChyan CDK in Settings before installing or updating.'));
            assert.ok(!f.calls.some(call => ['setup_app', 'update_to_version', 'send_notification_cmd'].includes(call.command)));
            assert.equal(f.operation(), null);
        } finally {await f.dispose();}
    });
}

test('Mirror upgrade with a saved CDK starts the update', async () => {
    const f = await fixture({update_source: 'mirrorchyan', installed: true, current_version: 'v1.4.8',
        available_versions: ['v1.4.8', 'v1.4.9'], installation: {source: 'mirrorchyan', version: 'v1.4.8'}});
    try {
        await f.click('Confirm Upgrade');
        assert.equal(f.operation().kind, 'mirror_update');
        assert.equal(f.calls.find(call => call.command === 'update_to_version').args.version, 'v1.4.9');
        assert.ok(f.button('Cancel'));
        await f.complete('succeeded');
    } finally {await f.dispose();}
});

for (const source of ['git', 'mirrorchyan']) test(`${source} update notes display the structured backend error message`, async () => {
    const f = await fixture({update_source: source, installed: true, current_version: 'v1.4.8',
        installation: {source, version: 'v1.4.8'}});
    try {
        f.fail('get_update_notes', 'Version check failed; check your connection');
        await f.publishApp({available_versions: ['v1.4.9']});
        assert.ok(f.container.textContent.includes('Failed to load notes: Version check failed; check your connection'));
        assert.ok(!f.container.textContent.includes('[object Object]'));
        assert.equal(f.button('Confirm Upgrade').disabled, true);
    } finally {await f.dispose();}
});

for (const [name, overrides] of [
    ['direct Git', {}],
    ['Mirror to Git', {installed: true, current_version: 'v1.4.8', installation: {source: 'mirrorchyan', version: 'v1.4.8'}}],
    ['Mirror', {update_source: 'mirrorchyan'}],
]) test(name + ': installation can return/reenter and retains cancellation state', async () => {
    const f = await fixture(overrides);
    try {
        await f.click('Install');
        assert.ok(f.container.textContent.includes('Installing App: ok-nte'));
        assert.ok(!f.container.textContent.includes('Upgrading...'));
        assert.ok(f.button('Cancel'));
        if (name === 'Mirror') {
            const progress = (phase, downloaded, total) => f.emit('mirror-update-progress', {
                app_name: 'ok-nte', operation_id: f.operation().id, phase, downloaded, total,
            });
            await progress('downloading', 100, 100);
            assert.equal(f.container.querySelector('[role="progressbar"][aria-valuenow]').getAttribute('aria-valuenow'), '70');
            await progress('preparing', 0, null);
            assert.equal(f.container.querySelector('[role="progressbar"][aria-valuenow]').getAttribute('aria-valuenow'), '70');
            await progress('extracting', 0, 200);
            await progress('extracting', 64, 200);
            assert.equal(f.container.querySelector('[role="progressbar"][aria-valuenow]').getAttribute('aria-valuenow'), '79');
            assert.ok(f.container.textContent.includes('Extracting update...'));
            await f.click('Back (Process Running)'); await f.click('Console');
            assert.equal(f.container.querySelector('[role="progressbar"][aria-valuenow]').getAttribute('aria-valuenow'), '79');
            await progress('extracting', 200, 200);
            assert.equal(f.container.querySelector('[role="progressbar"][aria-valuenow]').getAttribute('aria-valuenow'), '99');
            assert.ok(f.container.textContent.includes('Process in progress...'));
            assert.ok(f.button('Cancel'));
            await progress('installing', 0, null);
            assert.equal(f.container.querySelector('[role="progressbar"][aria-valuenow]').getAttribute('aria-valuenow'), '99');
        }
        await f.emit('app-log', {app_name: 'ok-nte', operation_id: f.operation().id, message: 'CURRENT INSTALL'});
        await f.publishApp({running: true}); // A stale scan must not route an installer to the runtime Console.
        await f.click('Back (Process Running)');
        assert.ok(!f.button('Cancel'));
        await f.click('Console');
        assert.ok(f.container.textContent.includes('Installing App: ok-nte'));
        assert.ok(f.container.textContent.includes('CURRENT INSTALL'));
        assert.ok(f.button('Cancel'));
        await f.click('Cancel');
        assert.equal(f.button('Cancel').disabled, true);
        assert.ok(f.container.textContent.includes('Process in progress...'));
        await f.click('Back (Process Running)');
        await f.click('Console');
        assert.equal(f.button('Cancel').disabled, true);
        assert.equal(f.calls.filter(call => call.command === 'cancel_app_operation').length, 1);
        await f.complete('cancelled');
        await f.publishApp({running: false, source_operation_state: 'failed', source_operation_kind: f.operation().kind, source_operation_error: 'stale failure'});
        assert.ok(f.container.textContent.includes('Operation cancelled.'));
        assert.ok(!f.button('Cancel'));
        await f.click('Done');
        assert.ok(!f.container.textContent.includes('Installation failed.'));
        await f.click('Console');
        assert.ok(f.container.textContent.includes('Operation cancelled.'));
    } finally {await f.dispose();}
});

test('old finished logs/events and previous invocation cannot finish a new task', async () => {
    const f = await fixture();
    try {
        await f.click('Install');
        const old = f.operation();
        await f.complete('cancelled');
        await f.click('Done');
        await f.click('Install');
        const current = f.operation(); assert.notEqual(current.id, old.id);
        await f.emit('app-log', {app_name: 'ok-nte', operation_id: old.id, message: 'OLD TASK', finished: true});
        await f.emit('app-operation', {...old, status: 'succeeded', can_cancel: false});
        await f.emit('app-log', {app_name: 'ok-nte', message: 'APPLICATION FINISHED', finished: true});
        assert.ok(f.button('Cancel'));
        assert.ok(f.container.textContent.includes('Process in progress...'));
        assert.ok(!f.container.textContent.includes('OLD TASK'));
    } finally {await f.dispose();}
});

test('a genuine failure after cancellation remains failed with its diagnosis', async () => {
    const f = await fixture();
    try {
        await f.click('Install'); await f.click('Cancel');
        await f.complete('failed', 'recovery failed: access denied');
        assert.ok(f.container.textContent.includes('There were errors.'));
        assert.ok(f.container.textContent.includes('recovery failed: access denied'));
        assert.ok(!f.container.textContent.includes('Operation cancelled.'));
    } finally {await f.dispose();}
});

test('Mirror settings renders the original information and error Alert text', async () => {
    const f = await fixture({update_source: 'mirrorchyan'});
    try {
        f.fail('mirrorchyan_has_cdk', 'Unreadable CDK');
        const settings = [...f.container.querySelectorAll('button')].find(button => button.querySelector('[data-testid="SettingsIcon"]'));
        await act(async () => settings.click());
        const info = [...f.container.querySelectorAll('.MuiAlert-message')].map(element => element.textContent);
        assert.ok(info.some(text => text.includes('Download complete or incremental updates through MirrorChyan. Save a CDK to install updates.')), 'Mirror information must be visible');
        assert.ok(info.includes('Could not update MirrorChyan settings.'), 'Mirror error text must be visible');
    } finally {await f.dispose();}
});

test('real Console ignores historical finished markers; outcome and Cancel come from the task', async () => {
    const f = await fixture();
    try {
        const Console = f.load('src/ConsolePage.tsx').default;
        const container = document.createElement('div'); document.body.append(container); const root = createRoot(container);
        let clicks = 0;
        await act(async () => root.render(React.createElement(Console, {title: 'Installing App: ok-nte', appName: 'ok-nte',
            logs: [{app_name: 'ok-nte', message: 'old session', finished: true}], isProcessing: true, onBack() {}, onCancel: async () => {clicks++;}})));
        const cancel = [...container.querySelectorAll('button')].find(button => button.textContent.trim() === 'Cancel');
        assert.ok(cancel); await act(async () => cancel.click()); assert.equal(clicks, 1);
        assert.ok(container.textContent.includes('Process in progress...'));
        await act(async () => root.unmount()); container.remove();
    } finally {await f.dispose();}
});


test('commit snapshots update app data after terminal events; stale snapshots and duplicate completion do not overwrite or notify twice', async () => {
    const f = await fixture({installed: true, current_version: 'v1.4.8', available_versions: ['v1.4.8', 'v1.4.9']});
    try {
        await f.click('Confirm Upgrade');
        const running = f.operation();
        await f.complete('succeeded');
        const terminal = f.operation();
        await f.publishApp({current_version: 'v1.4.9', operation: running});
        assert.ok(f.container.textContent.includes('Process finished.'));
        assert.ok([...f.container.querySelectorAll('.MuiChip-label')].some(element => element.textContent === 'v1.4.9'));
        assert.ok(!f.button('Cancel'));
        await f.emit('app', {...baseApp, revision: 1, installed: true, current_version: 'v1.4.8', operation: running});
        await f.emit('app-operation', terminal);
        await f.click('Done');
        await f.click('Console');
        assert.ok([...f.container.querySelectorAll('.MuiChip-label')].some(element => element.textContent === 'v1.4.9'));
        const notifications = f.calls.filter(call => call.command === 'send_notification_cmd');
        assert.equal(notifications.length, 2, 'one starting notification and one completion notification');
        assert.ok(notifications[1].args.body.includes('Release notes'));
    } finally {await f.dispose();}
});

test('persisted source failure retains its installation Console entry and diagnosis', async () => {
    const f = await fixture({installed: true, current_version: 'v1.4.8', source_operation_state: 'failed',
        source_operation_kind: 'git_install', source_operation_error: 'Cannot restore working',
        installation: {source: 'mirrorchyan', version: 'v1.4.8'}}, [
        {app_name: 'ok-nte', operation_id: 'older-task', message: 'OLDER TASK LOG'},
        {app_name: 'ok-nte', operation_id: 'failed-task', message: 'FAILED INSTALL LOG'},
        {app_name: 'ok-nte', message: 'UNRELATED APPLICATION LOG'},
    ]);
    try {
        await f.click('Console');
        assert.ok(f.container.textContent.includes('Installing App: ok-nte'));
        assert.ok(f.container.textContent.includes('Cannot restore working'));
        assert.ok(f.container.textContent.includes('FAILED INSTALL LOG'));
        assert.ok(!f.container.textContent.includes('OLDER TASK LOG'));
        assert.ok(!f.container.textContent.includes('UNRELATED APPLICATION LOG'));
        assert.ok(!f.button('Cancel'));
    } finally {await f.dispose();}
});

test('a failed application start reopens its runtime Console instead of the previous installation task', async () => {
    const f = await fixture();
    try {
        await f.click('Install'); await f.complete('succeeded');
        await f.publishApp({installed: true, current_version: 'v1.4.9'});
        await f.click('Done');
        f.fail('start_app', 'Application entry is unavailable');
        await f.click('Start App');
        assert.ok(f.container.textContent.includes('Application entry is unavailable'));
        await f.click('Done');
        await f.click('Console');
        assert.ok(f.container.textContent.includes('Console: ok-nte'));
        assert.ok(f.container.textContent.includes('Application entry is unavailable'));
        assert.ok(f.container.textContent.includes('There were errors.'));
        assert.ok(!f.container.textContent.includes('Installing App: ok-nte'));
    } finally {await f.dispose();}
});

test('profile configuration retains its own title and cancellable Console after returning home', async () => {
    const f = await fixture({installed: true, current_version: 'v1.4.9', profiles: [{name: 'China'}, {name: 'Global'}]});
    try {
        await f.click('Change Profile');
        const select = f.container.querySelector('[role="combobox"]');
        await act(async () => select.dispatchEvent(new MouseEvent('mousedown', {bubbles: true})));
        const option = document.querySelector('[role="option"][data-value="Global"]');
        assert.ok(option);
        await act(async () => option.click());
        await f.click('Change Profile');
        assert.equal(f.operation().kind, 'git_configure');
        assert.ok(f.container.textContent.includes("Changing Profile: ok-nte to 'Global'"));
        await f.click('Back (Process Running)'); await f.click('Console');
        assert.ok(f.container.textContent.includes("Changing Profile: ok-nte to 'Global'"));
        assert.ok(f.button('Cancel'));
    } finally {await f.dispose();}
});

test('Mirror to Git uses the existing profile picker before starting installation', async () => {
    const f = await fixture({installed: true, current_version: 'v1.4.9', profiles: [{name: 'China'}, {name: 'Global'}],
        installation: {source: 'mirrorchyan', version: 'v1.4.9'}});
    try {
        await f.click('Install');
        assert.ok(f.container.textContent.includes('Choose Profile for ok-nte'));
        assert.ok(!f.calls.some(call => call.command === 'setup_app'));
        const select = f.container.querySelector('[role="combobox"]');
        await act(async () => select.dispatchEvent(new MouseEvent('mousedown', {bubbles: true})));
        await act(async () => document.querySelector('[role="option"][data-value="Global"]').click());
        await f.click('Confirm & Install');
        assert.equal(f.calls.find(call => call.command === 'setup_app').args.profileName, 'Global');
        assert.equal(f.operation().kind, 'git_install');
        assert.ok(f.container.textContent.includes('Installing App: ok-nte'));
    } finally {await f.dispose();}
});

test('selected source reuses uninstalled controls while retaining the installed body in both directions', async () => {
    for (const source of ['git', 'mirrorchyan']) {
        const other = source === 'git' ? 'mirrorchyan' : 'git';
        const installation = {source: other, version: 'v1.4.9'};
        const f = await fixture({installed: true, current_version: 'v1.4.9', update_source: source, installation,
            profiles: [{name: 'China'}, {name: 'Global'}], available_versions: ['v1.4.9', 'v1.5.0']});
        try {
            assert.ok(f.button('Install'));
            for (const text of ['Start App', 'Stop App', 'Change Profile', 'Install selected update source', 'Confirm Upgrade']) assert.ok(!f.button(text), text);
            assert.ok(!f.container.textContent.includes('Auto Start'));
            assert.ok(!f.container.querySelector('[data-testid="DeleteIcon"]'));
            assert.ok(f.container.textContent.includes('(Not Installed)'));
            await f.publishApp({update_source: other});
            assert.ok(f.button('Start App'));
            assert.ok(!f.button('Install'));
            assert.ok([...f.container.querySelectorAll('.MuiChip-label')].some(element => element.textContent === installation.version));
            assert.equal(!!f.button('Change Profile'), other === 'git');
        } finally {await f.dispose();}
    }
});

test('restored tasks show only their logs; runtime Console does not load installation history', async () => {
    const operation = {id: 'restored', sequence: 1, app_name: 'ok-nte', kind: 'git_install', status: 'running', can_cancel: true};
    const saved = [
        {app_name: 'ok-nte', operation_id: 'old-mirror', message: 'OLD MIRROR INSTALL', finished: true},
        {app_name: 'ok-nte', operation_id: operation.id, message: 'CURRENT INSTALL'},
        {app_name: 'ok-nte', message: 'APPLICATION OUTPUT'},
    ];
    const f = await fixture({operation, running: true}, saved);
    try {
        await f.click('Back (Process Running)'); await f.click('Console');
        assert.ok(f.container.textContent.includes('Installing App: ok-nte'));
        assert.ok(f.container.textContent.includes('CURRENT INSTALL'));
        assert.ok(!f.container.textContent.includes('OLD MIRROR INSTALL'));
        assert.ok(!f.container.textContent.includes('APPLICATION OUTPUT'));
        await f.emit('app-operation', {...operation, sequence: 2, status: 'succeeded', can_cancel: false});
        await f.click('Done'); await f.click('Console');
        assert.ok(f.container.textContent.includes('Console: ok-nte'));
        assert.ok(f.container.textContent.includes('APPLICATION OUTPUT'));
        assert.ok(!f.container.textContent.includes('CURRENT INSTALL'));
    } finally {await f.dispose();}
});
