// Guard the upstream operation bodies while adding Mirror dispatch around them.
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

// Only these explicitly approved opt-in hooks may differ; every other token must match.
const replaceTokens = (text, from, to = '') => {
    // rustfmt can add a trailing comma to these explicitly listed call hooks.
    const escaped = tokens(from).replace(/[.*+?^${}()|[\]\\]/g, '\\$&').replace(/\\\)/g, ',?\\)');
    return text.replace(new RegExp(escaped, 'g'), to ? tokens(to) : '');
};
const withoutCancellation = input => {
    let text = tokens(input);
    for (const addition of ['use crate::extensions::cancellation;',
        'let token = cancellation::current_token(&app_name);', 'let token = cancellation::current_token(&app_name_for_messages);',
        'if token.as_ref().is_some_and(|token| !token.git_progress()) { return false; }',
        'cancellation::ensure_app_operation_not_cancelled()?;', 'let _recovery = cancellation::shield_recovery(app_name);',
        'if cancellation::current_kind(&app_name).is_some() { continue; }',
        '// Installation commands run Python/pip from this directory too.',
        'Err(e) if e.downcast_ref::<Error>().is_some_and(cancellation::is_cancelled) => Err(e),',
        'Err(e) if e.downcast_ref::<Error>().is_some_and(cancellation::is_cancelled) => { Err(e) }',
        'if cancellation::is_cancelled(&sync_error) { return Err(sync_error); }']) text = replaceTokens(text, addition);
    text = replaceTokens(text, 'cancellation::git_result(&app_name_for_task, fetch_result)', 'fetch_result');
    text = replaceTokens(text, 'let repo = cancellation::git_result(&app_name_for_messages, builder.clone(&url_for_clone_task, &repo_path_for_clone_task).with_context(|| format!("Git clone failed for {}", url_for_clone_task)))?;',
        'let repo = builder.clone(&url_for_clone_task, &repo_path_for_clone_task).with_context(|| format!("Git clone failed for {}", url_for_clone_task))?;');
    text = replaceTokens(text, 'cancellation::wait(app_name, client.get(url).send()).await?', 'client.get(url).send().await');
    text = replaceTokens(text, 'cancellation::wait(app_name, response.text()).await?', 'response.text().await');
    text = replaceTokens(text, 'cancellation::wait(app_name, futures_util::StreamExt::next(&mut stream)).await?', 'futures_util::StreamExt::next(&mut stream).await');
    text = replaceTokens(text, 'let extract_result = (|| -> Result<()> { extract_archive(&archive_path, &install_dir)?; Ok(()) })(); if let Err(extract_err) = extract_result {',
        'if let Err(extract_err) = extract_archive(&archive_path, &install_dir) {');
    text = replaceTokens(text, 'if let Some(token) = cancellation::current_token(app_name) { command::run_command_cancellable(pip_install_cmd, app_name, &pip_install_desc, token).await?; cancellation::begin_commit()?; } else { command::run_command_and_stream_output(pip_install_cmd, app_name, &pip_install_desc).await?; }',
        'command::run_command_and_stream_output(pip_install_cmd, app_name, &pip_install_desc).await?;');
    text = replaceTokens(text, 'if let Err(sync_error) = async { update_working_from_repo(app_name).await?; cancellation::ensure_app_operation_not_cancelled() }.await {',
        'if let Err(sync_error) = update_working_from_repo(app_name).await {');
    const recovery = 'if let Some(previous_version) = previous_version.as_deref() { rollback_to_previous_version(app_name, &repo_path, previous_version, previous_revision.as_deref(), "Operation cancelled by user").await?; }';
    text = replaceTokens(text, 'let commit_oid = match git::checkout_version_tag(app_name, &repo_path, version).await { Ok(oid) => oid, Err(error) => { let error: Error = error.into(); if cancellation::is_cancelled(&error) { ' + recovery + ' } return Err(error); } };',
        'let commit_oid = git::checkout_version_tag(app_name, &repo_path, version).await?;');
    text = replaceTokens(text, 'if let Err(cancelled) = cancellation::begin_commit() { ' + recovery + ' return Err(cancelled); }');
    text = replaceTokens(text, 'if cancellation::is_cancelled(&error) { persist_update_state(app_name, AppUpdateState::Idle, None, None).await?; emitter::emit_cancelled_finish(app_name); return Err(Error::Cancelled); }');
    // Approved pip retry marker lifecycle; source copy/delete and rollback stay upstream.
    text = replaceTokens(text, 'let pip_marker = working_dir_path.join(python_env::PIP_UPDATE_NEEDED_MARKER); let needs_pip_retry = pip_marker.try_exists()?;');
    text = replaceTokens(text, 'let sync_result = task::spawn_blocking', 'task::spawn_blocking');
    text = replaceTokens(text, '.await; // Synchronizing source files does not complete an interrupted pip installation. if needs_pip_retry && !pip_marker.try_exists()? { fs::File::create(&pip_marker).with_context(|| { format!("Failed to preserve pip retry marker {}", pip_marker.display()) })?; } sync_result??;', '.await??;');
    text = replaceTokens(text, 'let needs_pip_sync = !new_requirements_spec.is_empty() && (spec_changed || content_changed || working_dir_path.join(python_env::PIP_UPDATE_NEEDED_MARKER).try_exists()?);',
        'let needs_pip_sync = !new_requirements_spec.is_empty() && (spec_changed || content_changed);');
    text = replaceTokens(text, '} else if content_changed { let file_type', '} else { let file_type');
    text = replaceTokens(text, 'fs::File::create(&marker_path).with_context(|| { format!("Failed to create pip retry marker {}", marker_path.display()) })?;', 'fs::File::create(&marker_path).ok();');
    text = replaceTokens(text, 'if marker_path.try_exists()? { fs::remove_file(&marker_path).with_context(|| { format!("Failed to remove pip retry marker {}", marker_path.display()) })?; }', 'if marker_path.exists() { let _ = fs::remove_file(&marker_path); }');
    return text;
};

test('Git transport and Python environment match upstream except approved cancellation hooks', () => {
    for (const file of ['src-tauri/src/git.rs', 'src-tauri/src/python_env.rs']) assert.equal(withoutCancellation(read(file)), tokens(upstream(file)), file);
});

test('The upstream file utility equals upstream/HEAD in full', () => {
    const file = 'src-tauri/src/utils/file.rs';
    assert.equal(read(file), upstream(file));
});

test('Native Git update and recovery match upstream except approved cancellation boundaries', () => {
    for (const name of ['update_to_version_inner', 'rollback_to_previous_version', 'rollback_interrupted_pip_sync_on_startup',
        'update_working_from_repo', 'load_app_details', 'ensure_app_stopped_for_update', 'periodically_update_app_running_status']) {
        let expected = tokens(body(base, name));
        if (name === 'rollback_interrupted_pip_sync_on_startup') expected = replaceTokens(expected,
            'if marker_path.exists() { if let Err(e) = fs::remove_file(&marker_path) { warn!("Rollback for \'{}\' completed, but failed to remove marker {}: {}", app.name, marker_path.display(), e); } }');
        assert.equal(withoutCancellation(body(current, name)), expected, name);
    }
});

test('Pip retry is consumed only after pip succeeds, before the app is dispatched', () => {
    const pip = body(read('src-tauri/src/python_env.rs'), 'install_requirements');
    assert.ok(pip.indexOf('fs::File::create(&marker_path)') < pip.indexOf('command::run_command_cancellable'));
    assert.ok(pip.indexOf('fs::remove_file(&marker_path)') > pip.indexOf('command::run_command_and_stream_output'));
    assert.ok(tokens(pip).includes(tokens('fs::File::create(&marker_path).with_context(|| { format!("Failed to create pip retry marker {}", marker_path.display()) })?;')));
    assert.ok(tokens(pip).includes(tokens('command::run_command_cancellable(pip_install_cmd, app_name, &pip_install_desc, token).await?; cancellation::begin_commit()?;')));
    assert.ok(tokens(pip).includes(tokens('command::run_command_and_stream_output(pip_install_cmd, app_name, &pip_install_desc).await?;')));
    const start = body(current, 'start_app');
    assert.ok(start.indexOf('python_env::install_requirements(') < start.indexOf('execute_python::run_python_script('));
    assert.ok(start.slice(start.indexOf('python_env::install_requirements('), start.indexOf('let pyappify_version')).includes('.await?;'));
    assert.ok(!body(current, 'rollback_interrupted_pip_sync_on_startup').includes('fs::remove_file'));
});

test('Extracted setup retains the original copy, YAML, profile, Python and pip sequence', () => {
    const native = body(base, 'setup_app');
    const expected = native.slice(native.indexOf('    let working_dir_path'), native.indexOf('    let mut app_guard = APP.lock().await;'));
    const helper = body(current, 'setup_git_files_from_repository');
    const actual = helper.slice(helper.indexOf('    let working_dir_path'), helper.indexOf('    Ok(final_profile_name_to_set)'));
    assert.equal(withoutCancellation(actual), tokens(expected));
    assert.ok(body(current, 'setup_app').includes('app_dir_lock.lock().await'));
    assert.ok(body(current, 'setup_app').includes('operation.finish(&result)'));
});

test('Native update state and failure/retry wrapper remain after the source dispatch', () => {
    const native = body(base, 'update_to_version'), implementation = body(current, 'update_to_version_body');
    const beginning = '    ensure_app_stopped_for_update';
    assert.equal(withoutCancellation(implementation.slice(implementation.indexOf(beginning))), tokens(native.slice(native.indexOf(beginning))));
    assert.ok(body(current, 'update_to_version').includes('let _lock_guard = app_dir_lock.lock().await;'));
});

test('Optional root YAML override and embedded fallback equal upstream/HEAD', () => {
    const file = 'src-tauri/src/app.rs';
    assert.equal(tokens(body(read(file), 'read_embedded_app')), tokens(body(upstream(file), 'read_embedded_app')));
});

test('Git install keeps its version refresh inside the task; refresh rejects changed task identity', () => {
    const setup = body(current, 'setup_app_body');
    assert.ok(setup.includes('update_app_from_disk().await?'));
    const refresh = body(current, 'update_app_from_disk');
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
