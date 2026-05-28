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
import { useRetryConfig, useUpdateRetryConfig } from '@/hooks/use-metrics'
import { extractErrorMessage } from '@/lib/utils'

interface RuntimeConfigDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

/**
 * 运行时 Retry / Cooldown 配置对话框
 *
 * 修改后即时生效（共享 SharedRetryConfig），并写回 config.json。
 * 不重启即可让新策略生效，回退也只需要再改回来。
 */
export function RuntimeConfigDialog({ open, onOpenChange }: RuntimeConfigDialogProps) {
  const { data, isLoading, refetch } = useRetryConfig()
  const { mutate, isPending } = useUpdateRetryConfig()

  // 本地编辑态：空字符串视为"不指定"（None / 沿用代码内置默认）
  const [rateLimitSec, setRateLimitSec] = useState('')
  const [upstreamErrorSec, setUpstreamErrorSec] = useState('')
  const [overageSec, setOverageSec] = useState('')
  const [maxWaitSec, setMaxWaitSec] = useState('')
  const [maxWaitAttempts, setMaxWaitAttempts] = useState('')
  const [enabled, setEnabled] = useState(true)

  useEffect(() => {
    if (open && data) {
      setRateLimitSec(data.rateLimitCooldownSec?.toString() ?? '')
      setUpstreamErrorSec(data.upstreamErrorCooldownSec?.toString() ?? '')
      setOverageSec(data.overageRequestCooldownSec?.toString() ?? '')
      setMaxWaitSec(data.maxFallbackWaitSecs?.toString() ?? '')
      setMaxWaitAttempts(data.maxFallbackWaitAttempts?.toString() ?? '')
      setEnabled(data.transientCooldownEnabled)
    }
  }, [open, data])

  const parseOptional = (s: string): number | null | undefined => {
    const t = s.trim()
    if (!t) return null
    const n = Number(t)
    if (!Number.isInteger(n) || n <= 0) return undefined // 非法
    return n
  }

  const handleSave = () => {
    const rl = parseOptional(rateLimitSec)
    const ue = parseOptional(upstreamErrorSec)
    const ov = parseOptional(overageSec)
    const mw = parseOptional(maxWaitSec)
    const ma = parseOptional(maxWaitAttempts)
    if (rl === undefined) {
      toast.error('rateLimitCooldownSec 必须是正整数')
      return
    }
    if (ue === undefined) {
      toast.error('upstreamErrorCooldownSec 必须是正整数')
      return
    }
    if (ov === undefined) {
      toast.error('overageRequestCooldownSec 必须是正整数')
      return
    }
    if (rl !== null && rl > 600) {
      toast.error('rateLimitCooldownSec 不能超过 600 秒')
      return
    }
    if (ue !== null && ue > 600) {
      toast.error('upstreamErrorCooldownSec 不能超过 600 秒')
      return
    }
    if (ov !== null && ov > 7200) {
      toast.error('overageRequestCooldownSec 不能超过 7200 秒（2 小时）')
      return
    }
    if (mw === undefined) {
      toast.error('maxFallbackWaitSecs 必须是正整数')
      return
    }
    if (mw !== null && (mw < 3 || mw > 120)) {
      toast.error('maxFallbackWaitSecs 必须在 [3, 120] 秒范围内')
      return
    }
    if (ma === undefined) {
      toast.error('maxFallbackWaitAttempts 必须是正整数')
      return
    }
    if (ma !== null && (ma < 1 || ma > 10)) {
      toast.error('maxFallbackWaitAttempts 必须在 [1, 10] 范围内')
      return
    }

    mutate(
      {
        rateLimitCooldownSec: rl,
        upstreamErrorCooldownSec: ue,
        overageRequestCooldownSec: ov,
        transientCooldownEnabled: enabled,
        maxFallbackWaitSecs: mw,
        maxFallbackWaitAttempts: ma,
      },
      {
        onSuccess: () => {
          toast.success('运行时配置已生效并写回 config.json')
          refetch()
          onOpenChange(false)
        },
        onError: (e) => {
          toast.error(`更新失败: ${extractErrorMessage(e)}`)
        },
      },
    )
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>运行时 Retry / Cooldown 配置</DialogTitle>
          <DialogDescription>
            修改即时生效，无需重启。空白值表示沿用代码内置默认（429: 60s, 5xx: 10s, OVERAGE: 600s）。
          </DialogDescription>
        </DialogHeader>

        {isLoading ? (
          <div className="py-8 text-center text-muted-foreground">加载中…</div>
        ) : (
          <div className="space-y-4 py-2">
            <div className="space-y-2">
              <Label htmlFor="rl-sec">429 限流 cooldown（秒）</Label>
              <Input
                id="rl-sec"
                type="number"
                min={1}
                max={600}
                placeholder="60（默认）"
                value={rateLimitSec}
                onChange={(e) => setRateLimitSec(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                被 429 后该号短期不再被选中。±20% jitter 自动错峰恢复。
              </p>
            </div>

            <div className="space-y-2">
              <Label htmlFor="ue-sec">408/5xx cooldown（秒）</Label>
              <Input
                id="ue-sec"
                type="number"
                min={1}
                max={600}
                placeholder="10（默认）"
                value={upstreamErrorSec}
                onChange={(e) => setUpstreamErrorSec(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                上游 408/5xx 错误的冷却时长，通常远短于 429。
              </p>
            </div>

            <div className="space-y-2">
              <Label htmlFor="ov-sec">402 OVERAGE cooldown（秒）</Label>
              <Input
                id="ov-sec"
                type="number"
                min={1}
                max={7200}
                placeholder="600（默认 10 分钟）"
                value={overageSec}
                onChange={(e) => setOverageSec(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                开启 overage 付费后的短窗口（小时/天）速率上限。范围 [1, 7200]。受限号进冷却不被禁用，等待窗口刷新。
              </p>
            </div>

            <div className="rounded-lg border p-3 space-y-3">
              <div className="text-sm font-medium">全员 cooldown 智能等待</div>
              <p className="text-xs text-muted-foreground -mt-2">
                上游全部限流时让请求等到号恢复再返回，号池对客户端透明（不主动 502）。
              </p>
              <div className="space-y-2">
                <Label htmlFor="mw-sec">单轮等待上限（秒）</Label>
                <Input
                  id="mw-sec"
                  type="number"
                  min={3}
                  max={120}
                  placeholder="30（默认）"
                  value={maxWaitSec}
                  onChange={(e) => setMaxWaitSec(e.target.value)}
                />
                <p className="text-xs text-muted-foreground">
                  范围 [3, 120]。单次等待最多这么长；超过则走 fallback 借号。
                </p>
              </div>
              <div className="space-y-2">
                <Label htmlFor="mw-attempts">最大等待轮数</Label>
                <Input
                  id="mw-attempts"
                  type="number"
                  min={1}
                  max={10}
                  placeholder="3（默认）"
                  value={maxWaitAttempts}
                  onChange={(e) => setMaxWaitAttempts(e.target.value)}
                />
                <p className="text-xs text-muted-foreground">
                  范围 [1, 10]。同一次请求最多等这么多轮（每轮最长"单轮上限"）。
                </p>
              </div>
            </div>

            <div className="flex items-center justify-between rounded-lg border p-3">
              <div className="space-y-0.5">
                <Label htmlFor="cd-enabled" className="text-sm font-medium">
                  启用瞬态 Cooldown 机制
                </Label>
                <p className="text-xs text-muted-foreground">
                  关闭后退化为旧行为（仅释放 inflight，不切号）。仅排查问题时关闭。
                </p>
              </div>
              <Switch id="cd-enabled" checked={enabled} onCheckedChange={setEnabled} />
            </div>
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
