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
import { Settings2, Snowflake, Timer } from 'lucide-react'
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
      <DialogContent className="sm:max-w-lg max-h-[88vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Settings2 className="h-5 w-5 text-primary" />
            运行时 Retry / Cooldown 配置
          </DialogTitle>
          <DialogDescription>
            修改即时生效，无需重启，并写回 config.json。留空表示沿用内置默认。
          </DialogDescription>
        </DialogHeader>

        {isLoading ? (
          <div className="py-12 text-center text-sm text-muted-foreground">加载中…</div>
        ) : (
          <div className="space-y-4 py-1">
            {/* 冷却时长分区 */}
            <section className="rounded-xl border bg-muted/30 p-4 space-y-4">
              <div className="flex items-center gap-2 text-sm font-medium">
                <Snowflake className="h-4 w-4 text-blue-500" />
                各类错误冷却时长
              </div>

              <Field
                label="429 限流 cooldown"
                hint="被 429 后该号短期不再被选中，±20% jitter 自动错峰恢复"
                defaultLabel="默认 120s"
                unit="秒"
                min={1}
                max={600}
                value={rateLimitSec}
                onChange={setRateLimitSec}
              />
              <Field
                label="408 / 5xx cooldown"
                hint="上游服务端错误的冷却时长，通常远短于 429"
                defaultLabel="默认 30s"
                unit="秒"
                min={1}
                max={600}
                value={upstreamErrorSec}
                onChange={setUpstreamErrorSec}
              />
              <Field
                label="402 OVERAGE cooldown"
                hint="开启 overage 付费后的短窗口速率上限。受限号进冷却但不被禁用，等待窗口刷新"
                defaultLabel="默认 600s（10 分钟）"
                unit="秒"
                min={1}
                max={7200}
                value={overageSec}
                onChange={setOverageSec}
              />
            </section>

            {/* 智能等待分区 */}
            <section className="rounded-xl border bg-muted/30 p-4 space-y-4">
              <div className="flex items-center gap-2 text-sm font-medium">
                <Timer className="h-4 w-4 text-amber-500" />
                全员 cooldown 智能等待
              </div>
              <p className="text-xs text-muted-foreground -mt-2">
                上游全部限流时让请求等到号恢复再返回，对客户端透明（不主动 502）。
              </p>
              <Field
                label="单轮等待上限"
                hint="单次等待最多这么长；超过则走 fallback 借号"
                defaultLabel="默认 30s · 范围 [3,120]"
                unit="秒"
                min={3}
                max={120}
                value={maxWaitSec}
                onChange={setMaxWaitSec}
              />
              <Field
                label="最大等待轮数"
                hint="同一次请求最多等这么多轮（每轮最长「单轮上限」）"
                defaultLabel="默认 3 · 范围 [1,10]"
                unit="轮"
                min={1}
                max={10}
                value={maxWaitAttempts}
                onChange={setMaxWaitAttempts}
              />
            </section>

            {/* 总开关 */}
            <div className="flex items-center justify-between rounded-xl border p-4">
              <div className="space-y-0.5 pr-4">
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
            {isPending ? '保存中…' : '保存并生效'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

interface FieldProps {
  label: string
  hint: string
  defaultLabel: string
  unit: string
  min: number
  max: number
  value: string
  onChange: (v: string) => void
}

/** 统一的数值配置行：标签 + 默认值徽章 + 输入框 + 说明 */
function Field({ label, hint, defaultLabel, unit, min, max, value, onChange }: FieldProps) {
  return (
    <div className="space-y-1.5">
      <div className="flex items-center justify-between">
        <Label className="text-sm">{label}</Label>
        <span className="rounded-full bg-muted px-2 py-0.5 text-[10px] text-muted-foreground">
          {defaultLabel}
        </span>
      </div>
      <div className="relative">
        <Input
          type="number"
          min={min}
          max={max}
          placeholder="留空 = 默认"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          className="pr-10"
        />
        <span className="pointer-events-none absolute right-3 top-1/2 -translate-y-1/2 text-xs text-muted-foreground">
          {unit}
        </span>
      </div>
      <p className="text-xs text-muted-foreground leading-relaxed">{hint}</p>
    </div>
  )
}
