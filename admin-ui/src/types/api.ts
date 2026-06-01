// ===== Admin Metrics =====

export interface WindowRequestCounts {
  total: number
  success: number
  transientFail: number
  error: number
  /** 成功率百分比 0~100；total=0 时为 null */
  successRate: number | null
  /** 客户端最终看到错误的请求数（新增于 v2026.3.x） */
  clientVisibleErrors?: number
  /** 流式中断次数（新增于 v2026.3.x） */
  streamAborts?: number
  /** 流式中断后救活次数（新增于 v2026.3.x） */
  streamRecovers?: number
  inputTokensTotal?: number
  outputTokensTotal?: number
  cacheReadTokensTotal?: number
}

export interface WindowLatency {
  samples: number
  p50Ms: number
  p95Ms: number
  p99Ms: number
  /** 流式首字节延迟分位（新增于 v2026.3.x） */
  ttfbP50Ms?: number
  ttfbP95Ms?: number
  ttfbP99Ms?: number
  ttfbSamples?: number
}

/** 时间序列单点（1 分钟桶），新增于 v2026.3.x */
export interface TimeSeriesPoint {
  /** 桶相对 now 的秒偏移（负值，越小越旧） */
  tsOffsetSecs: number
  requestCount: number
  successCount: number
  inputTokens: number
  outputTokens: number
  cacheReadTokens: number
  p50Ms: number
  ttfbP50Ms: number
  fallbackProxyCount: number
}

/** 按 model / credential 维度切片的窗口统计 */
export interface DimensionBreakdown {
  /** 维度 key（model 名 / credential id 字符串） */
  key: string
  count: number
  success: number
  transientFail: number
  error: number
  successRate: number | null
  p50Ms: number
  p95Ms: number
  p99Ms: number
}

export interface AdminMetricsResponse {
  uptimeSeconds: number
  credentials: {
    total: number
    active: number
    cooling: number
    disabled: number
    successCountTotal: number
    transientFailureCountTotal: number
    failureCountTotal: number
  }
  requests: {
    last1m: WindowRequestCounts
    last5m: WindowRequestCounts
    /** 1 小时窗口（新增于 v2026.3.x） */
    last1h?: WindowRequestCounts
    allBuffer: WindowRequestCounts
  }
  latency: {
    last1m: WindowLatency
    last5m: WindowLatency
    /** 1 小时窗口（新增于 v2026.3.x） */
    last1h?: WindowLatency
    allBuffer: WindowLatency
  }
  cooldown: {
    currentlyCooling: number
    fallbackUsed1m: number
    waitedForCooldown1m: number
    fallbackUsed5m: number
    waitedForCooldown5m: number
    /** 1 小时窗口 fallback 触发数（新增于 v2026.3.x） */
    fallbackUsed1h?: number
    waitedForCooldown1h?: number
    /** tier 化代理 fallback 触发数（新增于 v2026.3.x） */
    fallbackProxyUsed1m?: number
    fallbackProxyUsed5m?: number
    fallbackProxyUsed1h?: number
  }
  bufferSize: number
  /** 中转层 prompt prefix cache 行为统计（旧版本可能缺失） */
  promptCache?: {
    enabled: boolean
    entries: number
    capacity: number
    ttlSecs: number
    hitTotal: number
    missTotal: number
    evictionTotal: number
    hitRate1m: number
    hitRate5m: number
    savedInputTokens5m: number
    reportedHitRate1m?: number
    reportedSavedInputTokens5m?: number
    perceivedCacheHitRatio?: number | null
  }
  /** 1 小时窗口按 model 切片（新增于 v2026.3.x，旧版本可能缺失） */
  byModel1h?: DimensionBreakdown[]
  /** 1 小时窗口按 credential id 切片（新增于 v2026.3.x，旧版本可能缺失） */
  byCredential1h?: DimensionBreakdown[]
  /** 过去 60 分钟时间序列（固定 60 点，新增于 v2026.3.x，旧版本可能缺失） */
  timeSeries60m?: TimeSeriesPoint[]
}

// ===== Prompt Cache 运行时配置 =====

export interface PromptCacheConfigPayload {
  enabled: boolean
  /** LRU 容量（条目数上限），范围 [1, 65536] */
  capacity: number
  /** 单条 entry TTL（秒），范围 [10, 86400]，默认 300（5min） */
  ttlSecs: number
  /** 当前 cache 中条目数（只读） */
  entries?: number
  hitTotal?: number
  missTotal?: number
  evictionTotal?: number
  /** 1 分钟窗口命中率（百分比，0~100） */
  hitRate1m?: number
  hitRate5m?: number
  savedInputTokens5m?: number
  /** 上报/计费口径系数；null/undefined 表示不干预 */
  perceivedCacheHitRatio?: number | null
  /** 1 分钟窗口上报/计费口径命中率（百分比，0~100） */
  reportedHitRate1m?: number
  /** 5 分钟内上报/计费口径节省 input tokens */
  reportedSavedInputTokens5m?: number
}

// ===== Retry 运行时配置 =====

export interface RetryConfigPayload {
  rateLimitCooldownSec?: number | null
  upstreamErrorCooldownSec?: number | null
  /**
   * 402 OVERAGE_REQUEST_LIMIT_EXCEEDED 的 cooldown 秒数（开启 overage 付费后的短窗口速率上限）。
   * 范围 [1, 7200]，默认 600（10 分钟）。**不**会禁用凭据，仅冷却等待 hour/day 窗口刷新。
   */
  overageRequestCooldownSec?: number | null
  transientCooldownEnabled: boolean
  /** 全员 cooldown 时智能等待的单轮上限（秒），范围 [3, 120]，默认 30 */
  maxFallbackWaitSecs?: number | null
  /** 单次 acquire 内"等待+重选"的最大轮数，范围 [1, 10]，默认 3 */
  maxFallbackWaitAttempts?: number | null
}

// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  currentId: number
  credentialGroups: CredentialGroupStatusItem[]
  credentials: CredentialStatusItem[]
}

export interface CredentialGroupStatusItem {
  id: string
  proxyUrl?: string
  hasProxy: boolean
}

// 单个凭据状态
export interface CredentialStatusItem {
  id: number
  priority: number
  disabled: boolean
  failureCount: number
  isCurrent: boolean
  expiresAt: string | null
  authMethod: string | null
  hasProfileArn: boolean
  email?: string
  refreshTokenHash?: string
  apiKeyHash?: string
  maskedApiKey?: string
  successCount: number
  lastUsedAt: string | null
  hasProxy: boolean
  proxyUrl?: string
  group?: string
  proxySource?: 'credential' | 'credential_direct' | 'group' | 'group_direct' | 'global' | 'none'
  refreshFailureCount: number
  disabledReason?: string
  endpoint: string
  /** 上游瞬态错误（429/408/5xx）累计次数（不参与禁用判定，仅供观测） */
  transientFailureCount?: number
  /** 最近一次瞬态错误时间（RFC3339） */
  lastTransientFailureAt?: string | null
  /** 当前冷却剩余秒数（0 或缺失表示不在冷却中） */
  cooldownRemainingSeconds?: number
  /** 当前冷却原因（"rate_limit" / "timeout" / "upstream_error" / "suspicious_activity"） */
  cooldownReason?: string
  /** 从 suspicious activity 响应体学习到的 directory key */
  directoryKey?: string
}

// 余额响应
export interface BalanceResponse {
  id: number
  subscriptionTitle: string | null
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  nextResetAt: number | null
}

// 成功响应
export interface SuccessResponse {
  success: boolean
  message: string
}

// 错误响应
export interface AdminErrorResponse {
  error: {
    type: string
    message: string
  }
}

// 请求类型
export interface SetDisabledRequest {
  disabled: boolean
}

export interface SetPriorityRequest {
  priority: number
}

export interface SetCredentialGroupRequest {
  group?: string | null
}

// 添加凭据请求
export interface AddCredentialRequest {
  refreshToken?: string
  authMethod?: 'social' | 'idc' | 'api_key'
  clientId?: string
  clientSecret?: string
  priority?: number
  authRegion?: string
  apiRegion?: string
  machineId?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  group?: string
  kiroApiKey?: string
  endpoint?: string
}

// 添加凭据响应
export interface AddCredentialResponse {
  success: boolean
  message: string
  credentialId: number
  email?: string
}

// ============ 系统提示词配置 ============

export type SystemPromptPosition = 'prepend' | 'append'

// 用户自定义预设
export interface UserPreset {
  id: string
  name: string
  description: string
  content: string
}

// 当前生效的系统提示词配置
export interface SystemPromptConfig {
  /** 注入总开关 */
  enabled: boolean
  /** 已启用的 preset id 列表（混合内置 + 用户自定义） */
  enabledPresets: string[]
  /** 用户自定义预设清单 */
  userPresets: UserPreset[]
  /** 自定义补充文本（可选） */
  content?: string
  position: SystemPromptPosition
  stripRestrictions: boolean
}

// 更新请求（所有字段可选，未提供则保持现状）
export interface UpdateSystemPromptRequest {
  enabled?: boolean
  /** Some([]) 视为禁用全部 preset */
  enabledPresets?: string[]
  /** "" 视为清空自定义文本 */
  content?: string
  position?: SystemPromptPosition
  stripRestrictions?: boolean
}

// 单个内置 preset 元数据 + 完整内容（前端本地拼接预览用）
export interface PresetMeta {
  id: string
  name: string
  description: string
  length: number
  content: string
}

export interface PresetCatalog {
  presets: PresetMeta[]
}

// 单个 preset 的完整内容（按 id 单独读取）
export interface PresetContent {
  id: string
  name: string
  content: string
}

// 创建用户预设请求
export interface CreateUserPresetRequest {
  id: string
  name: string
  description?: string
  content: string
}

// 更新用户预设请求（id 由 URL path 指定）
export interface UpdateUserPresetRequest {
  name?: string
  description?: string
  content?: string
}
