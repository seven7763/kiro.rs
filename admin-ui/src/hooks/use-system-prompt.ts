import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  getSystemPrompt,
  updateSystemPrompt,
  listPresets,
  getPresetContent,
  addUserPreset,
  updateUserPreset,
  deleteUserPreset,
} from '@/api/credentials'
import type {
  CreateUserPresetRequest,
  UpdateSystemPromptRequest,
  UpdateUserPresetRequest,
} from '@/types/api'

/** 查询当前生效的系统提示词配置 */
export function useSystemPrompt() {
  return useQuery({
    queryKey: ['systemPrompt'],
    queryFn: getSystemPrompt,
  })
}

/** 增量更新系统提示词配置 */
export function useUpdateSystemPrompt() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: UpdateSystemPromptRequest) => updateSystemPrompt(req),
    onSuccess: (data) => {
      queryClient.setQueryData(['systemPrompt'], data)
    },
  })
}

/** 查询内置 preset 元数据清单（静态数据，长缓存） */
export function usePresetCatalog() {
  return useQuery({
    queryKey: ['presetCatalog'],
    queryFn: listPresets,
    staleTime: Infinity,
  })
}

/** 按 id 一次性拉取 preset 完整内容（用于"预览"按钮） */
export function usePresetContent() {
  return useMutation({
    mutationFn: (id: string) => getPresetContent(id),
  })
}

/** 添加用户自定义预设 */
export function useAddUserPreset() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: CreateUserPresetRequest) => addUserPreset(req),
    onSuccess: (data) => {
      queryClient.setQueryData(['systemPrompt'], data)
    },
  })
}

/** 编辑用户自定义预设 */
export function useUpdateUserPreset() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, req }: { id: string; req: UpdateUserPresetRequest }) =>
      updateUserPreset(id, req),
    onSuccess: (data) => {
      queryClient.setQueryData(['systemPrompt'], data)
    },
  })
}

/** 删除用户自定义预设 */
export function useDeleteUserPreset() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => deleteUserPreset(id),
    onSuccess: (data) => {
      queryClient.setQueryData(['systemPrompt'], data)
    },
  })
}
