/**
 * 凭据批量导入归一化：企业导出 / KAM / external_idp / 平铺字段
 *
 * 批量导入 & KAM 导入共用此模块，避免两套解析逻辑漂移。
 */

export type ImportAuthMethod = 'social' | 'idc' | 'api_key' | 'external_idp'

export interface NormalizedCredential {
  refreshToken?: string
  accessToken?: string
  clientId?: string
  clientSecret?: string
  /** 仅用于错误提示，无法单独刷新 */
  clientIdHash?: string
  region?: string
  authRegion?: string
  apiRegion?: string
  priority?: number
  machineId?: string
  kiroApiKey?: string
  authMethod?: ImportAuthMethod | string
  endpoint?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  group?: string
  email?: string
  profileArn?: string
  tokenEndpoint?: string
  issuerUrl?: string
  scopes?: string
  audience?: string
  provider?: string
  expiresAt?: string
}

function asString(v: unknown): string | undefined {
  if (typeof v === 'string' && v.trim()) return v.trim()
  return undefined
}

function pickString(obj: Record<string, unknown> | undefined, ...keys: string[]): string | undefined {
  if (!obj) return undefined
  for (const k of keys) {
    const s = asString(obj[k])
    if (s) return s
  }
  return undefined
}

/** expiresAt: RFC3339 字符串，或 unix 秒/毫秒数字 */
export function normalizeExpiresAt(v: unknown): string | undefined {
  if (typeof v === 'string' && v.trim()) {
    const s = v.trim()
    // pure digits → unix
    if (/^\d{10,13}$/.test(s)) {
      const n = Number(s)
      const ms = s.length >= 13 ? n : n * 1000
      return new Date(ms).toISOString()
    }
    return s
  }
  if (typeof v === 'number' && Number.isFinite(v) && v > 0) {
    const ms = v > 1e12 ? v : v * 1000
    return new Date(ms).toISOString()
  }
  return undefined
}

function normalizeAuthMethod(raw: string | undefined): string | undefined {
  if (!raw) return undefined
  const lower = raw.trim().toLowerCase()
  if (lower === 'idc' || lower === 'builder-id' || lower === 'builderid' || lower === 'iam') {
    return 'idc'
  }
  if (lower === 'external_idp' || lower === 'externalidp' || lower === 'external-idp') {
    return 'external_idp'
  }
  if (lower === 'api_key' || lower === 'apikey' || lower === 'api-key') {
    return 'api_key'
  }
  if (lower === 'social') return 'social'
  return raw.trim()
}

function extractRegistration(obj: Record<string, unknown>): {
  clientId?: string
  clientSecret?: string
} {
  const nestedKeys = [
    'clientRegistration',
    'client_registration',
    'ssoRegistration',
    'sso_registration',
    'registration',
    'ssoCache',
    'sso_cache',
    'oidcClient',
    'oidc_client',
    // 有些导出把 registration 塞在 auth raw 旁
    'client',
    'oidc',
  ]
  for (const k of nestedKeys) {
    const n = obj[k]
    if (n && typeof n === 'object' && !Array.isArray(n)) {
      const rec = n as Record<string, unknown>
      const clientId = pickString(rec, 'clientId', 'client_id')
      const clientSecret = pickString(rec, 'clientSecret', 'client_secret')
      if (clientId || clientSecret) return { clientId, clientSecret }
    }
  }
  // 顶层已是 registration 形态（SSO cache 文件）
  const topId = pickString(obj, 'clientId', 'client_id')
  const topSecret = pickString(obj, 'clientSecret', 'client_secret')
  if (
    topId &&
    topSecret &&
    !pickString(obj, 'refreshToken', 'refresh_token') &&
    !pickString(obj, 'accessToken', 'access_token') &&
    !pickString(obj, 'kiroApiKey', 'kiro_api_key')
  ) {
    return { clientId: topId, clientSecret: topSecret }
  }
  return {}
}

/** 是否像 AWS SSO client registration 文件（无 token，仅 clientId/secret） */
export function isSsoClientRegistration(raw: unknown): boolean {
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return false
  const obj = raw as Record<string, unknown>
  const hasId = !!pickString(obj, 'clientId', 'client_id')
  const hasSecret = !!pickString(obj, 'clientSecret', 'client_secret')
  const hasToken = !!(
    pickString(obj, 'refreshToken', 'refresh_token') ||
    pickString(obj, 'accessToken', 'access_token') ||
    pickString(obj, 'kiroApiKey', 'kiro_api_key')
  )
  return hasId && hasSecret && !hasToken
}

/**
 * 从任意 JSON 根节点抽出「账号条目」列表。
 * 兼容：
 * - 单对象 / 数组
 * - KAM `{ version, accounts: [...] }`
 * - `{ credentials: [...] }` / `{ data: [...] }` / `{ items: [...] }` / `{ list: [...] }`
 * - 企业导出数组
 */
export function extractImportItems(parsed: unknown): unknown[] {
  if (parsed == null) return []
  if (Array.isArray(parsed)) return parsed

  if (typeof parsed === 'object') {
    const obj = parsed as Record<string, unknown>
    for (const key of ['accounts', 'credentials', 'data', 'items', 'list', 'users', 'records']) {
      const v = obj[key]
      if (Array.isArray(v)) return v
    }
    // 单账号对象（含企业导出 / KAM 平铺 / SSO token）
    return [parsed]
  }
  return []
}

/**
 * 数组里若同时出现「token 条目」和「纯 registration 条目」，
 * 按 clientIdHash 或唯一 registration 合并进 token（方便粘贴 sso/cache 两个文件拼成的数组）。
 */
export function mergeRegistrationsIntoTokens(items: unknown[]): unknown[] {
  const registrations: Array<Record<string, unknown>> = []
  const others: unknown[] = []

  for (const item of items) {
    if (isSsoClientRegistration(item)) {
      registrations.push(item as Record<string, unknown>)
    } else {
      others.push(item)
    }
  }
  if (registrations.length === 0) return items

  const byHash = new Map<string, Record<string, unknown>>()
  // registration 文件名式 hash 不在 JSON 内；只保留列表，唯一时全局合并
  for (const r of registrations) {
    const h = pickString(r, 'clientIdHash', 'client_id_hash')
    if (h) byHash.set(h, r)
  }

  const onlyReg = registrations.length === 1 ? registrations[0] : undefined

  return others.map((item) => {
    if (typeof item !== 'object' || item === null) return item
    const obj = item as Record<string, unknown>
    // 已有 clientId+secret 不必合并
    if (pickString(obj, 'clientId', 'client_id') && pickString(obj, 'clientSecret', 'client_secret')) {
      return item
    }
    const nested =
      obj.credentials && typeof obj.credentials === 'object'
        ? (obj.credentials as Record<string, unknown>)
        : undefined
    if (
      nested &&
      pickString(nested, 'clientId', 'client_id') &&
      pickString(nested, 'clientSecret', 'client_secret')
    ) {
      return item
    }

    const hash =
      pickString(obj, 'clientIdHash', 'client_id_hash') ||
      pickString(nested, 'clientIdHash', 'client_id_hash') ||
      (() => {
        const rawAuth =
          (obj.kiro_auth_token_raw as Record<string, unknown> | undefined) ||
          (obj.kiroAuthTokenRaw as Record<string, unknown> | undefined)
        return pickString(rawAuth, 'clientIdHash', 'client_id_hash')
      })()

    const reg = (hash && byHash.get(hash)) || onlyReg
    if (!reg) return item

    return {
      ...obj,
      clientId: pickString(obj, 'clientId', 'client_id') || pickString(reg, 'clientId', 'client_id'),
      clientSecret:
        pickString(obj, 'clientSecret', 'client_secret') ||
        pickString(reg, 'clientSecret', 'client_secret'),
      clientRegistration: {
        clientId: pickString(reg, 'clientId', 'client_id'),
        clientSecret: pickString(reg, 'clientSecret', 'client_secret'),
      },
    }
  })
}

/**
 * 解析导入 JSON 字符串 → 归一化后的凭据列表（批量 & KAM 共用）。
 */
export function parseAndNormalizeImportJson(raw: string): NormalizedCredential[] {
  const parsed = JSON.parse(raw)
  let items = extractImportItems(parsed)
  items = mergeRegistrationsIntoTokens(items)
  return items
    .map(normalizeCredentialInput)
    .filter((c) => !!(c.refreshToken?.trim() || c.kiroApiKey?.trim()))
}

/**
 * 将任意导出 JSON 单项归一化为导入字段。
 * 支持：
 * - kiro 企业导出（login_provider / kiro_auth_token_raw / kiro_profile_raw）
 * - 平铺 OAuth / IdC / external_idp
 * - 嵌套 clientRegistration
 */
export function normalizeCredentialInput(raw: unknown): NormalizedCredential {
  if (typeof raw !== 'object' || raw === null) return {}
  const obj = raw as Record<string, unknown>

  // credentials 嵌套（KAM 旧格式）
  const nestedCred =
    obj.credentials && typeof obj.credentials === 'object'
      ? (obj.credentials as Record<string, unknown>)
      : undefined

  const rawAuth =
    obj.kiro_auth_token_raw && typeof obj.kiro_auth_token_raw === 'object'
      ? (obj.kiro_auth_token_raw as Record<string, unknown>)
      : obj.kiroAuthTokenRaw && typeof obj.kiroAuthTokenRaw === 'object'
        ? (obj.kiroAuthTokenRaw as Record<string, unknown>)
        : undefined

  const rawProfile =
    obj.kiro_profile_raw && typeof obj.kiro_profile_raw === 'object'
      ? (obj.kiro_profile_raw as Record<string, unknown>)
      : obj.kiroProfileRaw && typeof obj.kiroProfileRaw === 'object'
        ? (obj.kiroProfileRaw as Record<string, unknown>)
        : undefined

  // usage raw 里可能有 email
  const rawUsage =
    obj.kiro_usage_raw && typeof obj.kiro_usage_raw === 'object'
      ? (obj.kiro_usage_raw as Record<string, unknown>)
      : obj.kiroUsageRaw && typeof obj.kiroUsageRaw === 'object'
        ? (obj.kiroUsageRaw as Record<string, unknown>)
        : undefined
  const usageUser =
    rawUsage?.userInfo && typeof rawUsage.userInfo === 'object'
      ? (rawUsage.userInfo as Record<string, unknown>)
      : undefined

  const reg = extractRegistration(obj)
  const regNested = nestedCred ? extractRegistration(nestedCred) : {}
  const regFromAuth = rawAuth ? extractRegistration(rawAuth) : {}

  const fromLayers = (...keys: string[]): string | undefined =>
    pickString(obj, ...keys) ||
    pickString(nestedCred, ...keys) ||
    pickString(rawAuth, ...keys)

  let authMethod = normalizeAuthMethod(fromLayers('authMethod', 'auth_method'))

  const loginProvider =
    fromLayers('login_provider', 'loginProvider', 'provider') ||
    pickString(rawAuth, 'provider')

  const tokenEndpoint = fromLayers('tokenEndpoint', 'token_endpoint')
  const issuerUrl = fromLayers('issuerUrl', 'issuer_url', 'startUrl', 'start_url')

  if (!authMethod) {
    const lp = loginProvider?.toLowerCase()
    if (lp === 'enterprise' || lp === 'internal' || lp === 'builderid') {
      authMethod = tokenEndpoint || issuerUrl ? 'external_idp' : 'idc'
    } else if (lp === 'externalidp' || lp === 'external_idp') {
      authMethod = 'external_idp'
    } else if (lp === 'google' || lp === 'github' || lp === 'builderid-social') {
      authMethod = 'social'
    }
  }

  // 有 tokenEndpoint/issuerUrl 且像 IdP 元数据 → external_idp
  if (!authMethod && (tokenEndpoint || issuerUrl)) {
    // AWS IDC startUrl (*.awsapps.com) 不是 external_idp
    const issuer = (tokenEndpoint || issuerUrl || '').toLowerCase()
    if (issuer.includes('awsapps.com') || issuer.includes('oidc.') || issuer.includes('amazonaws.com')) {
      authMethod = 'idc'
    } else {
      authMethod = 'external_idp'
    }
  }

  const clientId =
    fromLayers('clientId', 'client_id') ||
    reg.clientId ||
    regNested.clientId ||
    regFromAuth.clientId
  const clientSecret =
    fromLayers('clientSecret', 'client_secret') ||
    reg.clientSecret ||
    regNested.clientSecret ||
    regFromAuth.clientSecret

  // 有双 secret 但没写 authMethod → idc
  if (!authMethod && clientId && clientSecret) {
    authMethod = 'idc'
  }

  const region =
    fromLayers('region', 'idc_region', 'idcRegion', 'authRegion', 'auth_region') ||
    pickString(rawAuth, 'region')

  const profileArn =
    fromLayers('profileArn', 'profile_arn') ||
    pickString(rawProfile, 'arn', 'profileArn')

  const expiresAt =
    normalizeExpiresAt(obj.expiresAt) ||
    normalizeExpiresAt(obj.expires_at) ||
    normalizeExpiresAt(nestedCred?.expiresAt) ||
    normalizeExpiresAt(nestedCred?.expires_at) ||
    normalizeExpiresAt(rawAuth?.expiresAt) ||
    normalizeExpiresAt(rawAuth?.expires_at)

  const priority =
    typeof obj.priority === 'number'
      ? obj.priority
      : typeof nestedCred?.priority === 'number'
        ? (nestedCred.priority as number)
        : undefined

  return {
    refreshToken: fromLayers('refreshToken', 'refresh_token'),
    accessToken: fromLayers('accessToken', 'access_token'),
    clientId,
    clientSecret,
    clientIdHash: fromLayers('clientIdHash', 'client_id_hash'),
    region,
    authRegion: fromLayers('authRegion', 'auth_region') || region,
    apiRegion: fromLayers('apiRegion', 'api_region') || region,
    priority,
    machineId: fromLayers('machineId', 'machine_id'),
    kiroApiKey: fromLayers('kiroApiKey', 'kiro_api_key'),
    authMethod,
    endpoint: fromLayers('endpoint'),
    proxyUrl: fromLayers('proxyUrl', 'proxy_url'),
    proxyUsername: fromLayers('proxyUsername', 'proxy_username'),
    proxyPassword: fromLayers('proxyPassword', 'proxy_password'),
    group: fromLayers('group'),
    email:
      fromLayers('email') ||
      asString(obj.login_hint) ||
      asString(obj.loginHint) ||
      pickString(usageUser, 'email') ||
      asString(obj.nickname) ||
      asString(obj.label),
    profileArn,
    tokenEndpoint,
    issuerUrl,
    scopes: fromLayers('scopes'),
    audience: fromLayers('audience'),
    provider: loginProvider,
    expiresAt,
  }
}

/** 解析 authMethod 最终决策（导入提交前） */
export function resolveImportAuthMethod(cred: NormalizedCredential): ImportAuthMethod {
  const explicit = (cred.authMethod || '').trim().toLowerCase()
  if (explicit === 'external_idp' || explicit === 'externalidp') return 'external_idp'
  if (explicit === 'api_key' || explicit === 'apikey') return 'api_key'
  if (
    explicit === 'idc' ||
    explicit === 'builder-id' ||
    explicit === 'iam' ||
    explicit === 'builderid'
  ) {
    return 'idc'
  }
  if (cred.kiroApiKey?.trim()) return 'api_key'
  if (cred.tokenEndpoint?.trim() || cred.issuerUrl?.trim()) return 'external_idp'
  if (cred.clientId?.trim() && cred.clientSecret?.trim()) return 'idc'
  // Enterprise provider 无 secret 仍标 idc，让后续报错更准
  const p = (cred.provider || '').toLowerCase()
  if (p === 'enterprise' || p === 'internal' || p === 'builderid') return 'idc'
  return 'social'
}

/** 提交前校验，返回用户可读错误；null 表示通过 */
export function validateImportCredential(cred: NormalizedCredential): string | null {
  const method = resolveImportAuthMethod(cred)
  if (method === 'api_key') {
    if (!cred.kiroApiKey?.trim()) return '缺少 kiroApiKey'
    return null
  }
  if (!cred.refreshToken?.trim()) return '缺少 refreshToken'
  if (method === 'idc') {
    if (!cred.clientId?.trim() || !cred.clientSecret?.trim()) {
      const hash = cred.clientIdHash?.trim()
      if (hash) {
        return `idc/Enterprise 需要 clientId + clientSecret（导出只有 clientIdHash=${hash}；请在登录机打开 ~/.aws/sso/cache/${hash}.json 取出 clientId/clientSecret 一并导入，或在 JSON 中附上 clientRegistration 对象）`
      }
      return 'idc/Enterprise 需要 clientId + clientSecret（仅有 clientIdHash 无法在服务端刷新）'
    }
    return null
  }
  if (method === 'external_idp') {
    if (!cred.clientId?.trim()) return 'external_idp 需要 clientId'
    if (!cred.tokenEndpoint?.trim() && !cred.issuerUrl?.trim()) {
      return 'external_idp 需要 tokenEndpoint 或 issuerUrl'
    }
    return null
  }
  // social：若只给了其中一个 secret 字段
  if (cred.clientId?.trim() || cred.clientSecret?.trim()) {
    return 'idc 模式需要同时提供 clientId 和 clientSecret'
  }
  return null
}

/** 转为 Admin addCredential 请求体 */
export function toAddCredentialRequest(cred: NormalizedCredential): Record<string, unknown> {
  const authMethod = resolveImportAuthMethod(cred)
  if (authMethod === 'api_key') {
    return {
      authMethod: 'api_key',
      kiroApiKey: cred.kiroApiKey?.trim(),
      priority: cred.priority || 0,
      authRegion: cred.authRegion?.trim() || cred.region?.trim() || undefined,
      apiRegion: cred.apiRegion?.trim() || undefined,
      machineId: cred.machineId?.trim() || undefined,
      proxyUrl: cred.proxyUrl?.trim() || undefined,
      proxyUsername: cred.proxyUsername?.trim() || undefined,
      proxyPassword: cred.proxyPassword?.trim() || undefined,
      group: cred.group?.trim() || undefined,
      endpoint: cred.endpoint?.trim() || undefined,
      email: cred.email?.trim() || undefined,
    }
  }
  return {
    refreshToken: cred.refreshToken?.trim(),
    accessToken: cred.accessToken?.trim() || undefined,
    profileArn: cred.profileArn?.trim() || undefined,
    expiresAt: cred.expiresAt?.trim() || undefined,
    authMethod,
    authRegion: cred.authRegion?.trim() || cred.region?.trim() || undefined,
    apiRegion: cred.apiRegion?.trim() || undefined,
    region: cred.region?.trim() || undefined,
    clientId: cred.clientId?.trim() || undefined,
    clientSecret: cred.clientSecret?.trim() || undefined,
    clientIdHash: cred.clientIdHash?.trim() || undefined,
    tokenEndpoint: cred.tokenEndpoint?.trim() || undefined,
    issuerUrl: cred.issuerUrl?.trim() || undefined,
    scopes: cred.scopes?.trim() || undefined,
    audience: cred.audience?.trim() || undefined,
    provider: cred.provider?.trim() || undefined,
    email: cred.email?.trim() || undefined,
    priority: cred.priority || 0,
    machineId: cred.machineId?.trim() || undefined,
    proxyUrl: cred.proxyUrl?.trim() || undefined,
    proxyUsername: cred.proxyUsername?.trim() || undefined,
    proxyPassword: cred.proxyPassword?.trim() || undefined,
    group: cred.group?.trim() || undefined,
    endpoint: cred.endpoint?.trim() || undefined,
  }
}
