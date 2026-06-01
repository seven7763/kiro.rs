import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Switch } from '@/components/ui/switch'
import {
  useClearPromptCache,
  usePromptCacheConfig,
  useUpdatePromptCacheConfig,
} from '@/hooks/use-metrics'
import { extractErrorMessage } from '@/lib/utils'

interface PromptCacheDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

/**
 * Prompt Cache 运行时配置 + 监控面板
 *
 * 中转层自实现的 prefix 缓存：
 * - 解决上游 Kiro 协议不支持 Anthropic prompt caching 导致客户端命中率永远 0% 的问题。
 * - 命中时复用 conversation_id（让上游 session 缓存有机会生效）+ 上报 cache_read_input_tokens。
 * - 未命中时上报 cache_creation_input_tokens（首次创建）。
 */
export function PromptCacheDialog({ open, onOpenChange }: PromptCacheDialogProps) {
  const { data, isLoading, refetch } = usePromptCacheConfig()
  const { mutate, isPending } = useUpdatePromptCacheConfig()
  const { mutate: clearCache, isPending: isClearing } = useClearPromptCache()

  const [enabled, setEnabled] = useState(true)
  const [capacity, setCapacity] = useState('1024')
  const [ttlSecs, setTtlSecs] = useState('300')
  const [perceivedRatio, setPerceivedRatio] = useState('')

  useEffect(() => {
    if (open && data) {
      setEnabled(data.enabled)
      setCapacity(data.capacity.toString())
      setTtlSecs(data.ttlSecs.toString())
      setPerceivedRatio(
        data.perceivedCacheHitRatio === null || data.perceivedCacheHitRatio === undefined
          ? ''
          : data.perceivedCacheHitRatio.toString()
      )
    }
  }, [open, data])

  const parsePositiveInt = (s: string): number | undefined => {
    const t = s.trim()
    const n = Number(t)
    if (!Number.isInteger(n) || n <= 0) return undefined
    return n
  }

  const handleSave = () => {
    const cap = parsePositiveInt(capacity)
    const ttl = parsePositiveInt(ttlSecs)
    if (cap === undefined || cap < 1 || cap > 65536) {
      toast.error('capacity 必须在 [1, 65536] 范围内')
      return
    }
    if (ttl === undefined || ttl < 10 || ttl > 86400) {
      toast.error('ttlSecs 必须在 [10, 86400] 秒范围内')
      return
    }
    const ratioText = perceivedRatio.trim()
    const ratio =
      ratioText.length === 0 || ratioText.toLowerCase() === 'null'
        ? null
        : Number(ratioText)
    if (ratio !== null && (!Number.isFinite(ratio) || ratio < 0 || ratio > 0.95)) {
      toast.error('perceivedCacheHitRatio 必须在 [0.0, 0.95] 范围内，留空表示关闭')
      return
    }

    mutate(
      {
        enabled,
        capacity: cap,
        ttlSecs: ttl,
        perceivedCacheHitRatio: ratio,
      },
      {
        onSuccess: () => {
          toast.success('Prompt Cache 配置已生效并写回 config.json')
          refetch()
          onOpenChange(false)
        },
        onError: (e) => {
          toast.error(`更新失败: ${extractErrorMessage(e)}`)
        },
      }
    )
  }

  const handleClear = () => {
    clearCache(undefined, {
      onSuccess: () => {
        toast.success('Prompt Cache 已清空')
        refetch()
      },
      onError: (e) => {
        toast.error(`清空失败: ${extractErrorMessage(e)}`)
      },
    })
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-[520px]">
        <DialogHeader>
          <DialogTitle>Prompt Cache 配置</DialogTitle>
          <DialogDescription>
            中转层自实现的 prefix 缓存。真实缓存用于诊断，计费口径可单独设置为稳定的
            cache_read_input_tokens 比例，避免 cache_creation 溢价。
          </DialogDescription>
        </DialogHeader>

        {isLoading ? (
          <div className="text-sm text-muted-foreground py-6 text-center">加载中…</div>
        ) : (
          <div className="space-y-4 py-2">
            {/* 启用开关 */}
            <div className="flex items-center justify-between rounded-lg border p-3">
              <div className="space-y-0.5">
                <Label htmlFor="pc-enabled" className="text-sm font-medium">
                  启用 Prompt Cache
                </Label>
                <p className="text-xs text-muted-foreground">
                  关闭后所有请求都视为 miss（cache_*_input_tokens=0）。仅排查问题时关闭。
                </p>
              </div>
              <Switch id="pc-enabled" checked={enabled} onCheckedChange={setEnabled} />
            </div>

            {/* 容量 */}
            <div className="space-y-2">
              <Label htmlFor="pc-capacity">LRU 容量</Label>
              <Input
                id="pc-capacity"
                type="number"
                min={1}
                max={65536}
                value={capacity}
                onChange={(e) => setCapacity(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                范围 [1, 65536]，超容量时按 LRU 淘汰最久未访问的条目。默认 1024。
              </p>
            </div>

            {/* TTL */}
            <div className="space-y-2">
              <Label htmlFor="pc-ttl">条目 TTL（秒）</Label>
              <Input
                id="pc-ttl"
                type="number"
                min={10}
                max={86400}
                value={ttlSecs}
                onChange={(e) => setTtlSecs(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                范围 [10, 86400] 秒。默认 300（5 分钟，对齐 Anthropic ephemeral 规范）。
              </p>
            </div>

            <div className="space-y-2">
              <Label htmlFor="pc-perceived-ratio">计费命中率系数</Label>
              <Input
                id="pc-perceived-ratio"
                type="number"
                min={0}
                max={0.95}
                step={0.01}
                placeholder="留空关闭，例如 0.92"
                value={perceivedRatio}
                onChange={(e) => setPerceivedRatio(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                范围 [0.0, 0.95]。开启后客户端可见 input 内按该比例上报 cache_read，cache_creation 置 0。
              </p>
            </div>

            {/* 运行时统计 */}
            {data && (
              <div className="rounded-lg border p-3 space-y-2">
                <div className="text-sm font-medium">运行时统计</div>
                <div className="grid grid-cols-2 gap-2 text-xs text-muted-foreground">
                  <div>
                    当前条目数:{' '}
                    <b className="text-foreground">{data.entries ?? '—'}</b>
                  </div>
                  <div>
                    淘汰累计:{' '}
                    <b className="text-foreground">{data.evictionTotal ?? 0}</b>
                  </div>
                  <div>
                    命中累计:{' '}
                    <b className="text-foreground">{data.hitTotal ?? 0}</b>
                  </div>
                  <div>
                    未命中累计:{' '}
                    <b className="text-foreground">{data.missTotal ?? 0}</b>
                  </div>
                  <div>
                    计费命中率（1min）:{' '}
                    <b
                      className={
                        data.reportedHitRate1m && data.reportedHitRate1m >= 30
                          ? 'text-green-600'
                          : 'text-foreground'
                      }
                    >
                      {data.reportedHitRate1m !== undefined
                        ? `${data.reportedHitRate1m.toFixed(1)}%`
                        : '—'}
                    </b>
                  </div>
                  <div>
                    真实命中率（1min）:{' '}
                    <b
                      className={
                        data.hitRate1m && data.hitRate1m >= 30
                          ? 'text-green-600'
                          : 'text-foreground'
                      }
                    >
                      {data.hitRate1m !== undefined
                        ? `${data.hitRate1m.toFixed(1)}%`
                        : '—'}
                    </b>
                  </div>
                  <div>
                    真实命中率（5min）:{' '}
                    <b className="text-foreground">
                      {data.hitRate5m !== undefined
                        ? `${data.hitRate5m.toFixed(1)}%`
                        : '—'}
                    </b>
                  </div>
                  <div className="col-span-2">
                    5min 节省 input tokens:{' '}
                    <b className="text-foreground">
                      {data.savedInputTokens5m ?? 0}
                    </b>
                    <span className="text-muted-foreground">
                      {' '}
                      / 计费 {data.reportedSavedInputTokens5m ?? 0}
                    </span>
                  </div>
                  <div className="col-span-2">
                    计费系数:{' '}
                    <b className="text-foreground">
                      {data.perceivedCacheHitRatio === null ||
                      data.perceivedCacheHitRatio === undefined
                        ? '关闭'
                        : data.perceivedCacheHitRatio}
                    </b>
                  </div>
                </div>
                <div className="pt-1">
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={handleClear}
                    disabled={isClearing || isPending}
                  >
                    {isClearing ? '清空中…' : '清空 Cache'}
                  </Button>
                </div>
              </div>
            )}
          </div>
        )}

        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={isPending}>
            取消
          </Button>
          <Button onClick={handleSave} disabled={isPending || isLoading}>
            {isPending ? '保存中…' : '保存'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
