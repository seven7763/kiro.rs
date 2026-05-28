import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  clearPromptCache,
  getMetrics,
  getPromptCacheConfig,
  getRetryConfig,
  updatePromptCacheConfig,
  updateRetryConfig,
} from '@/api/credentials'
import type { PromptCacheConfigPayload, RetryConfigPayload } from '@/types/api'

/**
 * 拉取 Admin 聚合指标
 *
 * 5 秒轮询一次：满足"近 1 分钟成功率/延迟"实时观察需求，
 * 同时避免给后端造成额外负载（每次请求只是 O(buffer_len) 内存计算）。
 */
export function useMetrics() {
  return useQuery({
    queryKey: ['admin-metrics'],
    queryFn: getMetrics,
    refetchInterval: 5000,
    staleTime: 4000,
  })
}

/** 读取当前 retry 运行时配置 */
export function useRetryConfig() {
  return useQuery({
    queryKey: ['retry-config'],
    queryFn: getRetryConfig,
    staleTime: 60000,
  })
}

/** 更新 retry 配置（写入即时生效 + 持久化 config.json） */
export function useUpdateRetryConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: RetryConfigPayload) => updateRetryConfig(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['retry-config'] })
      queryClient.invalidateQueries({ queryKey: ['admin-metrics'] })
    },
  })
}

/** 读取 prompt cache 运行时配置 + 统计快照 */
export function usePromptCacheConfig() {
  return useQuery({
    queryKey: ['prompt-cache-config'],
    queryFn: getPromptCacheConfig,
    staleTime: 30000,
  })
}

/** 更新 prompt cache 配置（即时生效 + 持久化 config.json） */
export function useUpdatePromptCacheConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: PromptCacheConfigPayload) => updatePromptCacheConfig(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['prompt-cache-config'] })
      queryClient.invalidateQueries({ queryKey: ['admin-metrics'] })
    },
  })
}

/** 清空 prompt cache 全部条目（保留配置） */
export function useClearPromptCache() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: () => clearPromptCache(),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['prompt-cache-config'] })
      queryClient.invalidateQueries({ queryKey: ['admin-metrics'] })
    },
  })
}
