(() => {
  let pending = [], resolvers = new Map(), sequence = 0, result = null;
  const request = (method, args) => new Promise((resolve, reject) => {
    const id = ++sequence;
    resolvers.set(id, {resolve, reject});
    pending.push({id, method, args});
  });
  globalThis.state = {};
  const log = [];
  let logBytes = 0;
  const record = (level, args) => {
    if (logBytes >= 8192) return;
    const line = level + ': ' + args.map(value =>
      typeof value === 'string' ? value : JSON.stringify(preview(value))).join(' ');
    const text = line.length > 1024 ? line.slice(0, 1024) + '[truncated]' : line;
    logBytes += text.length;
    log.push(logBytes >= 8192 ? text + ' [log truncated]' : text);
  };
  globalThis.console = Object.freeze(Object.fromEntries(
    ['log', 'info', 'warn', 'error', 'debug', 'trace'].map(level =>
      [level, (...args) => record(level, args)])));
  globalThis.history = Object.freeze({read: args => request('history.read', args)});
  globalThis.rlm = Object.freeze({query: args => request('rlm.query', args)});
  globalThis.capabilities = Object.freeze({invoke: (name, args) => request('capability.invoke', {name, arguments: args})});
  globalThis.memory = Object.freeze(Object.fromEntries(
    ['recall', 'get', 'remember', 'supersede', 'forget'].map(name =>
      [name, args => request('memory.' + name, args)])));
  globalThis.__start = source => {
    if (result?.status === 'running') throw Error('cell already running');
    result = {status: 'running'};
    Promise.resolve().then(() => (new Function('return (async () => {' + source + '\n})()'))())
      .then(value => { result = {status: 'done', value: value ?? null}; },
            error => { result = {status: 'error', error: String(error)}; });
  };
  globalThis.__resume = response => {
    const resolver = resolvers.get(response.id);
    if (!resolver) throw Error('unknown request');
    resolvers.delete(response.id);
    if (response.error) resolver.reject(Error(response.error));
    else resolver.resolve(response.value);
  };
  const preview = value => {
    let remaining = 3000;
    const seen = new WeakSet();
    const visit = (value, depth) => {
      if (remaining <= 0 || depth > 6) return '[truncated]';
      if (typeof value === 'string') {
        const limit = Math.min(remaining, 1024);
        const text = value.length > limit ? value.slice(0, limit) + '[truncated]' : value;
        remaining -= JSON.stringify(text).length;
        return text;
      }
      if (value === null || typeof value !== 'object') {
        remaining -= 24;
        return value;
      }
      if (seen.has(value)) return '[circular]';
      seen.add(value);
      const output = Array.isArray(value) ? [] : {};
      const keys = Object.keys(value);
      let count = 0;
      for (const key of keys) {
        if (remaining <= 0 || count >= 32 || key.length > 128) {
          if (Array.isArray(output)) output.push('[truncated]');
          else output['[truncated]'] = true;
          break;
        }
        remaining -= JSON.stringify(key).length + 4;
        output[key] = visit(value[key], depth + 1);
        count++;
      }
      return output;
    };
    return visit(value, 0);
  };
  // Printed output travels with whatever the cell produces next, so a
  // model sees its own diagnostics without a separate channel.
  const drain = () => log.length ? {log: log.splice(0, log.length)} : {};
  let saved = null;
  globalThis.checkpoint = values => {
    if (values === null || typeof values !== 'object' || Array.isArray(values)) throw Error('checkpoint requires named JSON values');
    const encoded = JSON.stringify(values, (_key, value) => {
      if (typeof value === 'number' && !Number.isFinite(value)) throw Error('checkpoint requires finite numbers');
      if (typeof value === 'function' || typeof value === 'symbol' || typeof value === 'undefined') throw Error('checkpoint requires JSON values');
      return value;
    });
    if (encoded.length > 32 * 1024) throw Error('checkpoint exceeds 32 KiB');
    saved = {version: 1, state: JSON.parse(encoded)};
    return saved;
  };
  globalThis.__poll = () => pending.length
      ? {status: 'request', ...pending.shift(), ...drain()}
    : result?.status === 'done'
      ? {status: 'done', value: preview(result.value), checkpoint: saved, ...drain()}
    : result?.status === 'error' ? {...result, ...drain()} : result;
})();
