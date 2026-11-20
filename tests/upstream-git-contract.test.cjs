// Guard upstream Git/Python behavior and the fork's integration boundaries.
// Run offline in a checkout with upstream/HEAD available.
const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const {execFileSync} = require('node:child_process');
const read = file => fs.readFileSync(file, 'utf8').replace(/\r\n/g, '\n');
const upstream = file => execFileSync('git', ['-c', 'core.fsmonitor=false', 'show', 'upstream/HEAD:' + file], {encoding: 'utf8'}).replace(/\r\n/g, '\n');
const body = (text, name) => {
    const match = text.match(new RegExp('^(?:pub(?:\\(crate\\))? )?(?:async )?fn ' + name + '\\b[\\s\\S]*?^}', 'm'));
    assert.ok(match, `Missing function ${name}`);
    return match[0].replace(/^pub(?:\(crate\))? /, '');
};
// Preserve string literals; ignore only formatting and visibility differences.
const tokens = text => text.match(/"(?:\\.|[^"\\])*"|\S/g).join('');
const file = 'src-tauri/src/app_service.rs';
const current = read(file), base = upstream(file);


const strip = (input, ...hooks) => hooks.reduce((result, hook) => result.replace(tokens(hook), ''), tokens(input));
const appFunction = name => body(current, name);
const rustFile = name => `src-tauri/src/${name}.rs`;

function inOrder(source, ...parts) {
    let offset = 0;
    for (const part of parts) {
        const position = source.indexOf(part, offset);
        assert.ok(position >= 0, `Missing or out of order: ${part}`);
        offset = position + part.length;
    }
}

test('Git checkout layout, release selection and changelog parsing retain upstream behavior', () => {
    const git = rustFile('git');
    for (const name of ['open_repository', 'ensure_bare_layout', 'migrate_to_bare_layout',
        'checkout_head_to_working', 'checkout_detached', 'checkout_existing_revision',
        'parse_version_tag', 'select_kept_tags', 'sync_tags', 'notes_in_language']) {
        assert.equal(tokens(body(read(git), name)), tokens(body(upstream(git), name)), name);
    }
    for (const name of ['utils/file', 'changelog']) {
        const path = rustFile(name);
        assert.equal(read(path), upstream(path), path);
    }
});

test('The shared Git fetch and progress callback preserve upstream logic with cancellation hooks', () => {
    const git = rustFile('git');
    const fetch = body(read(git), 'fetch_from_origin');
    const normalized = strip(fetch,
        'cancellation::ensure_app_operation_not_cancelled()?;',
        'else if let Some(token) = cancellation::current_token(app_name) { callbacks.transfer_progress(move |_| token.git_progress()); }')
        .replace(tokens('cancellation::git_result(app_name, result)'), 'result');
    assert.equal(normalized, tokens(body(upstream(git), 'fetch_from_origin')));
    const progress = body(read(git), 'create_transfer_progress_callback');
    assert.equal(strip(progress,
        'let token = cancellation::current_token(&app_name);',
        'if token.as_ref().is_some_and(|token| !token.git_progress()) { return false; }'),
        tokens(body(upstream(git), 'create_transfer_progress_callback')));
});

test('Python extraction retains the upstream fallback and archive handling', () => {
    const python = rustFile('python_env');
    for (const name of ['get_download_urls', 'extract_archive', 'extract_zip', 'extract_tar',
        'setup_python_env', 'clean_python_install']) {
        assert.equal(tokens(body(read(python), name)), tokens(body(upstream(python), name)), name);
    }
    const download = body(read(python), 'download_file');
    inOrder(download, 'cancellation::wait(app_name, client.get(url).send())',
        'cancellation::wait(app_name, response.text())',
        'cancellation::wait(app_name, futures_util::StreamExt::next(&mut stream))');
});

test('Pip uses the task-owned process job and consumes its retry marker only after success', () => {
    const python = read(rustFile('python_env'));
    const pip = body(python, 'install_requirements');
    inOrder(pip, 'fs::File::create(&marker_path)', 'let mut result = run_pip_install(',
        '!cancellation::app_operation_cancelled()', 'result = run_pip_install(',
        'result?;', 'cancellation::begin_commit()?;', 'fs::remove_file(&marker_path)');
    const runPip = body(python, 'run_pip_install');
    inOrder(runPip, 'command::run_command_and_capture_output(', 'cancellation::is_cancelled(&error)', 'pip_mirror::is_network_error');
    const command = read(rustFile('utils/command'));
    inOrder(body(command, 'run_command_and_capture_output'), 'cancellation::current_token(app_name)', 'run_command(');
    inOrder(body(command, 'run_command'), 'ProcessJob::new()', 'attach_and_resume(&child)', 'token.cancelled()', 'job.wait_empty()');
    const start = appFunction('start_app');
    inOrder(start, 'python_env::install_requirements(', '.await?;', 'execute_python::run_python_script(');
    assert.ok(!appFunction('rollback_interrupted_pip_sync_on_startup').includes('fs::remove_file'));
});

test('Git setup retains the upstream file preparation, profile selection and Python/pip sequence', () => {
    const native = body(base, 'setup_app_at_version');
    const begin = '    let working_dir_path';
    const expected = native.slice(native.indexOf(begin), native.indexOf('    let saved_app'));
    const helper = appFunction('setup_git_files_from_repository');
    const actual = helper.slice(helper.indexOf(begin), helper.indexOf('    Ok(final_profile_name_to_set)'));
    assert.equal(strip(actual, 'cancellation::ensure_app_operation_not_cancelled()?;',
        'cancellation::ensure_app_operation_not_cancelled()?;', 'cancellation::ensure_app_operation_not_cancelled()?;'), tokens(expected));
    inOrder(body(read(rustFile('extensions/setup')), 'setup_app_with_version'),
        'app_dir_lock.lock().await', 'setup_git_app(', 'operation.finish(&result)');
});

test('Native updates preserve the upstream state wrapper and recover before reporting cancellation', () => {
    const native = body(base, 'update_to_version');
    const implementation = appFunction('update_to_version_body');
    const beginning = '    ensure_app_stopped_for_update';
    const cancellation = `if cancellation::is_cancelled(&error) {
        persist_update_state(app_name, AppUpdateState::Idle, None, None).await?;
        emitter::emit_cancelled_finish(app_name);
        return Err(Error::Cancelled);
    }`;
    assert.equal(strip(implementation.slice(implementation.indexOf(beginning)), cancellation),
        tokens(native.slice(native.indexOf(beginning))));
    inOrder(appFunction('update_to_version'), 'app_dir_lock.lock().await', 'update_to_version_body(', 'operation.finish(&result)');
    inOrder(appFunction('update_to_version_inner'), 'git::checkout_version_tag(',
        'cancellation::ensure_app_operation_not_cancelled()', 'rollback_to_previous_version(',
        'cancellation::is_cancelled(&sync_error)', 'cancellation::begin_commit()');
    inOrder(appFunction('rollback_to_previous_version'), 'cancellation::shield_recovery(app_name)',
        'git::checkout_existing_revision(', 'update_and_save_app(');
});

test('Conversion backs up the original body before Git can write into working', () => {
    const convert = body(read(rustFile('source_switch')), 'setup_git_from_mirror');
    inOrder(convert, 'begin(&install_root', 'prepare_body(&mut transaction', 'git::ensure_repository(',
        'git::checkout_version_tag(', 'setup_git_files_from_repository(',
        'prepare_commit(', 'cancellation::begin_commit()', 'transaction.apply(');
});

test('Optional root YAML override and embedded fallback equal upstream/HEAD', () => {
    const file = 'src-tauri/src/app.rs';
    assert.equal(tokens(body(read(file), 'read_embedded_app')), tokens(body(upstream(file), 'read_embedded_app')));
});

test('Git install keeps its version refresh inside the task; refresh rejects changed task identity', () => {
    const setup = body(current, 'setup_git_app');
    assert.ok(setup.includes('update_app_from_disk_at_version(version).await?'));
    const refresh = body(current, 'update_app_from_disk_at_version');
    assert.ok(refresh.includes('let querying_operation = cancellation::current_id(&app.name);'));
    assert.ok(refresh.includes('cancellation::current_id(&app.name) != querying_operation'));
    assert.ok(!refresh.includes('cancellation::current_kind(&app.name).is_some()'));
});

test('Only confirmed startup consumes its notice; ordinary Mirror queries cannot overwrite it', () => {
    const start = body(current, 'start_app');
    const confirmed = start.indexOf('if check_running_on_start(&app_name).await? {');
    const consume = start.indexOf('started.consume_startup_notice');
    assert.ok(consume > confirmed);
    assert.ok(confirmed > start.indexOf('execute_python::run_python_script('));
    assert.ok(confirmed > start.indexOf('execute_python::run_frozen_app('));
    assert.ok(!start.slice(start.lastIndexOf('} else {')).includes('consume_startup_notice'));
    assert.ok(start.indexOf('save_app_config_to_json(&started).await?') < start.indexOf('*app = started'));
    assert.ok(!body(read('src-tauri/src/mirror/service.rs'), 'refresh_release').includes('.update_note ='));
});
