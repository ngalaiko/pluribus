(() => {
  let pending = [], resolvers = new Map(), sequence = 0, result = null;
  const request = (method, args) => new Promise((resolve, reject) => {
    const id = ++sequence;
    resolvers.set(id, {resolve, reject});
    pending.push({id, method, args});
  });
  globalThis.state = {};
  const ARRAY_PROTO = Array.prototype;
  const OBJECT_PROTO = Object.prototype;
  const arrayIsArray = Array.isArray;
  const objectCreate = Object.create;
  const objectGetPrototypeOf = Object.getPrototypeOf;
  const objectGetOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
  const objectDefineProperty = Object.defineProperty;
  const reflectOwnKeys = Reflect.ownKeys;
  const numberIsFinite = Number.isFinite;
  const jsonParse = JSON.parse;
  const jsonStringify = JSON.stringify;
  const WeakSetConstructor = WeakSet;
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
  globalThis.history = Object.freeze({
    read: args => request('history.read', args),
    search: args => request('history.search', args),
  });
  globalThis.rlm = Object.freeze({query: args => request('rlm.query', args)});
  globalThis.capabilities = Object.freeze({invoke: (name, args) => request('capability.invoke', {name, arguments: args})});
  globalThis.__start = source => {
    if (result?.status === 'running') throw Error('cell already running');
    if (!explicitMode) saved = null;
    warnings = [];
    result = {status: 'running'};
    Promise.resolve().then(() => (new Function('return (async () => {' + source + '\n})()'))())
      .then(value => {
        if (!explicitMode && saved === null) {
          try {
            saved = encodeCheckpoint(globalThis.state, 'automatic');
          } catch (error) {
            warnings.push(('working state was not checkpointed: ' + String(error)).slice(0, 256));
          }
        }
        result = {status: 'done', value: value ?? null};
      },
            error => { result = {status: 'error', error: String(error)}; });
  };
  globalThis.__resume = response => {
    const resolver = resolvers.get(response.id);
    if (!resolver) throw Error('unknown request');
    resolvers.delete(response.id);
    if (response.error) resolver.reject(Error(response.error));
    else resolver.resolve(response.value);
  };
  globalThis.__restoreCheckpoint = checkpoint => {
    if (checkpoint === null || typeof checkpoint !== 'object' ||
        checkpoint.version !== 1 || checkpoint.state === null ||
        typeof checkpoint.state !== 'object' || arrayIsArray(checkpoint.state)) {
      throw Error('unsupported working checkpoint');
    }
    if (checkpoint.mode === 'automatic') {
      explicitMode = false;
      saved = null;
    } else if (checkpoint.mode === 'explicit' || checkpoint.mode === undefined) {
      explicitMode = true;
      saved = encodeCheckpoint(checkpoint.state, 'explicit');
    } else {
      throw Error('unsupported working checkpoint mode');
    }
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
  const MAX_CHECKPOINT_BYTES = 32 * 1024;
  const hasOwn = OBJECT_PROTO.hasOwnProperty;
  const define = (object, key, value) => objectDefineProperty(object, key, {
    configurable: true, enumerable: true, value, writable: true,
  });
  const utf8Bytes = text => {
    let bytes = 0;
    for (let index = 0; index < text.length; index++) {
      const code = text.charCodeAt(index);
      if (code <= 0x7f) bytes += 1;
      else if (code <= 0x7ff) bytes += 2;
      else if (code >= 0xd800 && code <= 0xdbff && index + 1 < text.length &&
               text.charCodeAt(index + 1) >= 0xdc00 && text.charCodeAt(index + 1) <= 0xdfff) {
        bytes += 4;
        index++;
      } else bytes += 3;
    }
    return bytes;
  };
  const checkpointFailure = (path, reason) => {
    const boundedPath = path.length > 128 ? path.slice(0, 128) + '[truncated]' : path;
    throw Error(`checkpoint ${reason} at ${boundedPath}`);
  };
  const encodeCheckpoint = (values, mode) => {
    if (values === null || typeof values !== 'object' || Array.isArray(values)) {
      throw Error('checkpoint requires named JSON values');
    }
    const seen = new WeakSetConstructor();
    const budget = {bytes: 0};
    const addBytes = amount => {
      budget.bytes += amount;
      if (budget.bytes > MAX_CHECKPOINT_BYTES) throw Error('checkpoint exceeds 32 KiB');
    };
    const jsonStringBytes = text => {
      let bytes = 2;
      for (let index = 0; index < text.length; index++) {
        const code = text.charCodeAt(index);
        if (code === 0x22 || code === 0x5c || code === 0x08 || code === 0x09 ||
            code === 0x0a || code === 0x0c || code === 0x0d) bytes += 2;
        else if (code < 0x20) bytes += 6;
        else if (code >= 0xd800 && code <= 0xdbff && index + 1 < text.length &&
                 text.charCodeAt(index + 1) >= 0xdc00 && text.charCodeAt(index + 1) <= 0xdfff) {
          bytes += 4;
          index++;
        } else if (code >= 0xd800 && code <= 0xdfff) bytes += 6;
        else if (code <= 0x7f) bytes++;
        else if (code <= 0x7ff) bytes += 2;
        else bytes += 3;
        if (bytes > MAX_CHECKPOINT_BYTES) throw Error('checkpoint exceeds 32 KiB');
      }
      return bytes;
    };
    const visit = (value, path, depth) => {
      if (depth > 128) throw Error('checkpoint exceeds 128 nesting levels');
      if (value === null) {
        addBytes(4);
        return value;
      }
      if (typeof value === 'string') {
        addBytes(jsonStringBytes(value));
        return value;
      }
      if (typeof value === 'boolean') {
        addBytes(value ? 4 : 5);
        return value;
      }
      if (typeof value === 'number') {
        if (!numberIsFinite(value)) checkpointFailure(path, 'requires finite numbers');
        addBytes(utf8Bytes(jsonStringify(value)));
        return value;
      }
      if (typeof value === 'undefined' || typeof value === 'function' ||
          typeof value === 'symbol' || typeof value === 'bigint') {
        checkpointFailure(path, 'contains an unsupported value');
      }
      if (typeof value !== 'object') checkpointFailure(path, 'contains an unsupported value');
      if (seen.has(value)) checkpointFailure(path, 'contains a cycle');
      seen.add(value);

      const array = arrayIsArray(value);
      const prototype = objectGetPrototypeOf(value);
      if (array) {
        if (prototype !== ARRAY_PROTO && prototype !== null) {
          checkpointFailure(path, 'contains an unsupported prototype');
        }
        const output = [];
        addBytes(1);
        for (let index = 0; index < value.length; index++) {
          if (!hasOwn.call(value, index)) checkpointFailure(`${path}[${index}]`, 'contains a hole');
          const descriptor = objectGetOwnPropertyDescriptor(value, String(index));
          if (!descriptor || !('value' in descriptor) || !descriptor.enumerable) {
            checkpointFailure(`${path}[${index}]`, 'contains an unsupported property');
          }
          if (index > 0) addBytes(1);
          define(output, String(index), visit(descriptor.value, `${path}[${index}]`, depth + 1));
        }
        for (const key of reflectOwnKeys(value)) {
          if (key === 'length') continue;
          if (typeof key !== 'string' || !/^(0|[1-9][0-9]*)$/.test(key) || Number(key) >= value.length) {
            checkpointFailure(path, 'contains an unsupported property');
          }
        }
        addBytes(1);
        seen.delete(value);
        return output;
      }
      if (prototype !== OBJECT_PROTO && prototype !== null) {
        checkpointFailure(path, 'contains an unsupported prototype');
      }
      const output = objectCreate(null);
      addBytes(1);
      let first = true;
      for (const key of reflectOwnKeys(value)) {
        if (typeof key !== 'string') checkpointFailure(path, 'contains a symbol property');
        const descriptor = objectGetOwnPropertyDescriptor(value, key);
        if (!descriptor || !('value' in descriptor) || !descriptor.enumerable) {
          checkpointFailure(`${path}.${key}`, 'contains an unsupported property');
        }
        if (!first) addBytes(1);
        first = false;
        addBytes(jsonStringBytes(key));
        addBytes(1);
        define(output, key, visit(descriptor.value, `${path}.${key}`, depth + 1));
      }
      addBytes(1);
      seen.delete(value);
      return output;
    };
    const normalized = visit(values, 'state', 0);
    const encoded = jsonStringify(normalized);
    if (utf8Bytes(encoded) > MAX_CHECKPOINT_BYTES) throw Error('checkpoint exceeds 32 KiB');
    const checkpoint = {version: 1, state: jsonParse(encoded)};
    if (mode !== undefined) checkpoint.mode = mode;
    return checkpoint;
  };
  let saved = null;
  let explicitMode = false;
  let warnings = [];
  globalThis.checkpoint = values => {
    saved = encodeCheckpoint(values, 'explicit');
    explicitMode = true;
    return saved;
  };
  globalThis.__poll = () => pending.length
      ? {status: 'request', ...pending.shift(), ...drain()}
    : result?.status === 'done'
      ? {status: 'done', value: preview(result.value), checkpoint: saved, warnings, ...drain()}
    : result?.status === 'error' ? {...result, ...drain()} : result;
})();
