// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  currentId: number
  credentials: CredentialStatusItem[]
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
  refreshFailureCount: number
  disabledReason?: string
  endpoint: string
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
