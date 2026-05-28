import { useMetrics } from '@/hooks/use-metrics'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import {
  Activity,
  AlertTriangle,
  CheckCircle2,
  Clock,
  Database,
  Snowflake,
  Timer,
} from 'lucide-react'

/**
 * 顶部全局健康面板
 *
 * 7 张卡片：
 * 1. 凭据健康（活跃 / 冷却 / 禁用 / 总数）
 * 2. 1min 成功率 + 总样本数
 * 3. 1min 瞬态率（>10% 红色高亮）
 * 4. P50 / P95 延迟
 * 5. 1min cooldown 触发次数 + 智能等待次数
 * 6. uptime
 * 7. Prompt Cache 命中率（1min）+ 5min 节省 input tokens
 *
 * 5 秒自动刷新（轮询 /api/admin/metrics）。无数据时显示 "—"。
 */
export function HealthBanner() {
  const { data, isLoading } = useMetrics()

  const formatPercent = (v: number | null | undefined) =>
    v === null || v === undefined ? '—' : `${v.toFixed(1)}%`
  const formatMs = (v: number | undefined) =>
    v === undefined ? '—' : v >= 1000 ? `${(v / 1000).toFixed(1)}s` : `${v}ms`
  const formatUptime = (secs: number | undefined) => {
    if (secs === undefined) return '—'
    const d = Math.floor(secs / 86400)
    const h = Math.floor((secs % 86400) / 3600)
    const m = Math.floor((secs % 3600) / 60)
    if (d > 0) return `${d}d ${h}h`
    if (h > 0) return `${h}h ${m}m`
    return `${m}m`
  }

  const c = data?.credentials
  const r1 = data?.requests.last1m
  const l1 = data?.latency.last1m
  const cd = data?.cooldown
  const pc = data?.promptCache

  const formatSavedTokens = (v: number | undefined) => {
    if (v === undefined) return '—'
    if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(1)}M`
    if (v >= 1_000) return `${(v / 1_000).toFixed(1)}k`
    return v.toString()
  }

  // 瞬态率：transient_fail / total
  const transientRate =
    r1 && r1.total > 0
      ? (r1.transientFail / r1.total) * 100
      : null
  const transientWarn = transientRate !== null && transientRate > 10

  return (
    <div className="grid gap-3 md:grid-cols-3 lg:grid-cols-4 xl:grid-cols-7 mb-6">
      {/* 1. 凭据健康 */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-xs font-medium text-muted-foreground flex items-center gap-1.5">
            <Activity className="h-3.5 w-3.5" />
            凭据健康
          </CardTitle>
        </CardHeader>
        <CardContent>
          <div className="text-2xl font-bold leading-none">
            <span className="text-green-600">{c?.active ?? '—'}</span>
            <span className="text-sm text-muted-foreground"> / {c?.total ?? '—'}</span>
          </div>
          <div className="text-xs text-muted-foreground mt-1.5 space-x-2">
            <span>冷却 <b className="text-amber-600">{c?.cooling ?? 0}</b></span>
            <span>禁用 <b className="text-rose-600">{c?.disabled ?? 0}</b></span>
          </div>
        </CardContent>
      </Card>

      {/* 2. 成功率（近 1min） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-xs font-medium text-muted-foreground flex items-center gap-1.5">
            <CheckCircle2 className="h-3.5 w-3.5" />
            成功率（1min）
          </CardTitle>
        </CardHeader>
        <CardContent>
          <div
            className={`text-2xl font-bold leading-none ${
              r1 && r1.successRate !== null && r1.successRate < 95
                ? 'text-rose-600'
                : 'text-green-600'
            }`}
          >
            {formatPercent(r1?.successRate)}
          </div>
          <div className="text-xs text-muted-foreground mt-1.5">
            {r1?.success ?? 0}/{r1?.total ?? 0} 请求
          </div>
        </CardContent>
      </Card>

      {/* 3. 瞬态率 */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-xs font-medium text-muted-foreground flex items-center gap-1.5">
            <AlertTriangle className="h-3.5 w-3.5" />
            瞬态率（1min）
          </CardTitle>
        </CardHeader>
        <CardContent>
          <div
            className={`text-2xl font-bold leading-none ${
              transientWarn ? 'text-amber-600' : 'text-foreground'
            }`}
          >
            {formatPercent(transientRate)}
          </div>
          <div className="text-xs text-muted-foreground mt-1.5">
            {r1?.transientFail ?? 0} 次 429/5xx
          </div>
        </CardContent>
      </Card>

      {/* 4. 延迟 P50/P95 */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-xs font-medium text-muted-foreground flex items-center gap-1.5">
            <Timer className="h-3.5 w-3.5" />
            延迟（1min）
          </CardTitle>
        </CardHeader>
        <CardContent>
          <div className="text-2xl font-bold leading-none">{formatMs(l1?.p50Ms)}</div>
          <div className="text-xs text-muted-foreground mt-1.5">
            P95 <b>{formatMs(l1?.p95Ms)}</b>
          </div>
        </CardContent>
      </Card>

      {/* 5. cooldown 行为 */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-xs font-medium text-muted-foreground flex items-center gap-1.5">
            <Snowflake className="h-3.5 w-3.5" />
            冷却行为（1min）
          </CardTitle>
        </CardHeader>
        <CardContent>
          <div className="text-2xl font-bold leading-none">
            {cd?.fallbackUsed1m ?? 0}
            <span className="text-sm text-muted-foreground"> fallback</span>
          </div>
          <div className="text-xs text-muted-foreground mt-1.5">
            智能等待 <b>{cd?.waitedForCooldown1m ?? 0}</b>
          </div>
        </CardContent>
      </Card>

      {/* 6. Uptime */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-xs font-medium text-muted-foreground flex items-center gap-1.5">
            <Clock className="h-3.5 w-3.5" />
            运行时长
          </CardTitle>
        </CardHeader>
        <CardContent>
          <div className="text-2xl font-bold leading-none">
            {formatUptime(data?.uptimeSeconds)}
          </div>
          <div className="text-xs text-muted-foreground mt-1.5">
            {isLoading ? '加载中...' : `buffer ${data?.bufferSize ?? 0}`}
          </div>
        </CardContent>
      </Card>

      {/* 7. Prompt Cache 命中率（中转层自实现） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-xs font-medium text-muted-foreground flex items-center gap-1.5">
            <Database className="h-3.5 w-3.5" />
            Prompt Cache (1min)
          </CardTitle>
        </CardHeader>
        <CardContent>
          <div
            className={`text-2xl font-bold leading-none ${
              pc && pc.enabled
                ? pc.hitRate1m >= 30
                  ? 'text-green-600'
                  : 'text-foreground'
                : 'text-muted-foreground'
            }`}
          >
            {pc?.enabled ? `${pc.hitRate1m.toFixed(0)}%` : '禁用'}
          </div>
          <div className="text-xs text-muted-foreground mt-1.5">
            5min 节省 <b>{formatSavedTokens(pc?.savedInputTokens5m)}</b> tok
          </div>
        </CardContent>
      </Card>
    </div>
  )
}
