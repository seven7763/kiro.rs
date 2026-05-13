import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  CredentialsStatusResponse,
  BalanceResponse,
  SuccessResponse,
  SetDisabledRequest,
  SetPriorityRequest,
  AddCredentialRequest,
  AddCredentialResponse,
  SystemPromptConfig,
  UpdateSystemPromptRequest,
  PresetCatalog,
  PresetContent,
  CreateUserPresetRequest,
  UpdateUserPresetRequest,
} from '@/types/api'

// 创建 axios 实例
const api = axios.create({
  baseURL: '/api/admin',
  headers: {
    'Content-Type': 'application/json',
  },
})

// 请求拦截器添加 API Key
api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  return config
})

// 获取所有凭据状态
export async function getCredentials(): Promise<CredentialsStatusResponse> {
  const { data } = await api.get<CredentialsStatusResponse>('/credentials')
  return data
}

// 设置凭据禁用状态
export async function setCredentialDisabled(
  id: number,
  disabled: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/disabled`,
    { disabled } as SetDisabledRequest
  )
  return data
}

// 设置凭据优先级
export async function setCredentialPriority(
  id: number,
  priority: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/priority`,
    { priority } as SetPriorityRequest
  )
  return data
}

// 重置失败计数
export async function resetCredentialFailure(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/reset`)
  return data
}

// 强制刷新 Token
export async function forceRefreshToken(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/refresh`)
  return data
}

// 获取凭据余额
export async function getCredentialBalance(id: number): Promise<BalanceResponse> {
  const { data } = await api.get<BalanceResponse>(`/credentials/${id}/balance`)
  return data
}

// 添加新凭据
export async function addCredential(
  req: AddCredentialRequest
): Promise<AddCredentialResponse> {
  const { data } = await api.post<AddCredentialResponse>('/credentials', req)
  return data
}

// 删除凭据
export async function deleteCredential(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/credentials/${id}`)
  return data
}

// 获取负载均衡模式
export async function getLoadBalancingMode(): Promise<{ mode: 'priority' | 'balanced' }> {
  const { data } = await api.get<{ mode: 'priority' | 'balanced' }>('/config/load-balancing')
  return data
}

// 设置负载均衡模式
export async function setLoadBalancingMode(mode: 'priority' | 'balanced'): Promise<{ mode: 'priority' | 'balanced' }> {
  const { data } = await api.put<{ mode: 'priority' | 'balanced' }>('/config/load-balancing', { mode })
  return data
}

// ============ 系统提示词配置 ============

// 获取当前系统提示词配置
export async function getSystemPrompt(): Promise<SystemPromptConfig> {
  const { data } = await api.get<SystemPromptConfig>('/config/system-prompt')
  return data
}

// 更新系统提示词配置（增量更新）
export async function updateSystemPrompt(
  req: UpdateSystemPromptRequest
): Promise<SystemPromptConfig> {
  const { data } = await api.put<SystemPromptConfig>('/config/system-prompt', req)
  return data
}

// 列出所有内置 preset（元数据，不含 content）
export async function listPresets(): Promise<PresetCatalog> {
  const { data } = await api.get<PresetCatalog>('/config/system-prompt/presets')
  return data
}

// 获取单个 preset 的完整内容（"预览"按钮）
export async function getPresetContent(id: string): Promise<PresetContent> {
  const { data } = await api.get<PresetContent>(
    `/config/system-prompt/presets/${encodeURIComponent(id)}`
  )
  return data
}

// 添加用户自定义预设（返回更新后的 SystemPromptConfig）
export async function addUserPreset(
  req: CreateUserPresetRequest
): Promise<SystemPromptConfig> {
  const { data } = await api.post<SystemPromptConfig>(
    '/config/system-prompt/user-presets',
    req
  )
  return data
}

// 编辑用户自定义预设
export async function updateUserPreset(
  id: string,
  req: UpdateUserPresetRequest
): Promise<SystemPromptConfig> {
  const { data } = await api.put<SystemPromptConfig>(
    `/config/system-prompt/user-presets/${encodeURIComponent(id)}`,
    req
  )
  return data
}

// 删除用户自定义预设
export async function deleteUserPreset(id: string): Promise<SystemPromptConfig> {
  const { data } = await api.delete<SystemPromptConfig>(
    `/config/system-prompt/user-presets/${encodeURIComponent(id)}`
  )
  return data
}
