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
import { Badge } from '@/components/ui/badge'
import { Network, Plus, Pencil, Trash2, ArrowLeft } from 'lucide-react'
import {
  useCredentialGroups,
  useUpsertCredentialGroup,
  useDeleteCredentialGroup,
  useCredentials,
} from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'
import type { CredentialGroupStatusItem } from '@/types/api'

interface GroupManagerDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

type View =
  | { mode: 'list' }
  | { mode: 'create' }
  | { mode: 'edit'; group: CredentialGroupStatusItem }

/**
 * 凭据分组管理对话框
 *
 * 列出所有分组（id + 代理来源 + 挂靠数），支持新建 / 编辑代理 / 删除。
 * 分组代理改动即时生效（后端从 token_manager 可变状态读分组），无需重启。
 * 删除时挂靠该组的凭据会回落本机直连。
 */
export function GroupManagerDialog({ open, onOpenChange }: GroupManagerDialogProps) {
  const [view, setView] = useState<View>({ mode: 'list' })

  // 对话框关闭时重置回列表视图
  useEffect(() => {
    if (!open) setView({ mode: 'list' })
  }, [open])

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg max-h-[88vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Network className="h-5 w-5 text-primary" />
            凭据分组管理
          </DialogTitle>
          <DialogDescription>
            为一组凭据指定统一出口（SOCKS5 / HTTP 代理或直连）。改动即时生效，无需重启。
          </DialogDescription>
        </DialogHeader>

        {view.mode === 'list' ? (
          <GroupList
            onCreate={() => setView({ mode: 'create' })}
            onEdit={(group) => setView({ mode: 'edit', group })}
            onClose={() => onOpenChange(false)}
          />
        ) : (
          <GroupForm
            existing={view.mode === 'edit' ? view.group : undefined}
            onBack={() => setView({ mode: 'list' })}
          />
        )}
      </DialogContent>
    </Dialog>
  )
}

interface GroupListProps {
  onCreate: () => void
  onEdit: (group: CredentialGroupStatusItem) => void
  onClose: () => void
}

/** 分组列表：每行展示 id、出口来源、挂靠凭据数，附编辑/删除操作 */
function GroupList({ onCreate, onEdit, onClose }: GroupListProps) {
  const { data: groups, isLoading } = useCredentialGroups()
  const { data: credData } = useCredentials()
  const del = useDeleteCredentialGroup()

  // 统计每个分组挂靠的凭据数（用于删除确认提示）
  const countByGroup = (id: string) =>
    credData?.credentials.filter((c) => c.group === id).length ?? 0

  const handleDelete = (group: CredentialGroupStatusItem) => {
    const n = countByGroup(group.id)
    const warn =
      n > 0
        ? `该组下 ${n} 个凭据将回落本机直连。`
        : ''
    if (!confirm(`确定删除分组 "${group.id}"？${warn}此操作无法撤销。`)) return
    del.mutate(group.id, {
      onSuccess: (res) => toast.success(res.message),
      onError: (e) => toast.error(`删除失败: ${extractErrorMessage(e)}`),
    })
  }

  return (
    <div className="space-y-3 py-1">
      {isLoading ? (
        <div className="py-10 text-center text-sm text-muted-foreground">加载中…</div>
      ) : !groups || groups.length === 0 ? (
        <div className="rounded-xl border border-dashed py-10 text-center text-sm text-muted-foreground">
          暂无分组。新建一个分组后，可在凭据卡片的「移动到分组」里把号挂进来。
        </div>
      ) : (
        <div className="space-y-2">
          {groups.map((group) => {
            const count = countByGroup(group.id)
            return (
              <div
                key={group.id}
                className="flex items-center justify-between rounded-xl border bg-muted/30 p-3"
              >
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="truncate font-medium">{group.id}</span>
                    <Badge variant={group.hasProxy ? 'default' : 'secondary'} className="text-[10px]">
                      {group.hasProxy ? '代理' : '直连'}
                    </Badge>
                    {group.hasUsername && (
                      <Badge variant="secondary" className="text-[10px]">
                        鉴权
                      </Badge>
                    )}
                  </div>
                  <div className="mt-0.5 truncate text-xs text-muted-foreground">
                    {group.hasProxy ? group.proxyUrl : '不走代理（含全局代理也忽略）'}
                    <span className="ml-2">· {count} 个凭据</span>
                  </div>
                </div>
                <div className="flex shrink-0 gap-1">
                  <Button variant="ghost" size="icon" title="编辑" onClick={() => onEdit(group)}>
                    <Pencil className="h-4 w-4" />
                  </Button>
                  <Button
                    variant="ghost"
                    size="icon"
                    title="删除"
                    className="text-destructive hover:text-destructive"
                    disabled={del.isPending}
                    onClick={() => handleDelete(group)}
                  >
                    <Trash2 className="h-4 w-4" />
                  </Button>
                </div>
              </div>
            )
          })}
        </div>
      )}

      <DialogFooter className="gap-2 sm:gap-2">
        <Button variant="ghost" onClick={onClose}>
          关闭
        </Button>
        <Button onClick={onCreate}>
          <Plus className="h-4 w-4 mr-1" />
          新建分组
        </Button>
      </DialogFooter>
    </div>
  )
}

interface GroupFormProps {
  /** 存在则为编辑模式（id 不可改）；否则为新建模式 */
  existing?: CredentialGroupStatusItem
  onBack: () => void
}

/** 新建 / 编辑分组表单 */
function GroupForm({ existing, onBack }: GroupFormProps) {
  const isEdit = !!existing
  const upsert = useUpsertCredentialGroup()

  const [id, setId] = useState(existing?.id ?? '')
  // 编辑时 proxyUrl 可能是 undefined（直连），回显空串
  const [proxyUrl, setProxyUrl] = useState(existing?.proxyUrl ?? '')
  // 用户名非密钥，编辑时回显原值；留空提交即清空（与后端对称语义一致）
  const [username, setUsername] = useState(existing?.proxyUsername ?? '')
  // 密码：编辑模式留空 = 保留原密码；新建模式留空 = 无密码
  const [password, setPassword] = useState('')

  const handleSave = () => {
    const trimmedId = id.trim()
    if (!trimmedId) {
      toast.error('分组 ID 不能为空')
      return
    }
    const trimmedUrl = proxyUrl.trim()
    // 非直连时给个 scheme 友好校验（仅提示，不强制阻断 direct/空）
    if (
      trimmedUrl &&
      trimmedUrl.toLowerCase() !== 'direct' &&
      !/^(socks5|socks5h|http|https):\/\//i.test(trimmedUrl)
    ) {
      toast.error('代理 URL 需以 socks5:// 或 http:// 开头（直连请留空）')
      return
    }

    upsert.mutate(
      {
        id: trimmedId,
        // 留空 = 直连（后端把空串规整为 None）
        proxyUrl: trimmedUrl || undefined,
        // 用户名 WYSIWYG：编辑时已回显原值，框里是什么就存什么（空串=清空）。
        proxyUsername: username.trim(),
        // 密码无法回显：编辑模式留空 = 不传（保留原密码）；非空 = 更新。
        // 新建模式留空 = 无密码。
        proxyPassword: password ? password : undefined,
      },
      {
        onSuccess: (res) => {
          toast.success(res.message)
          onBack()
        },
        onError: (e) => toast.error(`保存失败: ${extractErrorMessage(e)}`),
      },
    )
  }

  return (
    <div className="space-y-4 py-1">
      <div className="space-y-1.5">
        <Label className="text-sm">分组 ID</Label>
        <Input
          placeholder="例如 socks-hk"
          value={id}
          disabled={isEdit}
          onChange={(e) => setId(e.target.value)}
        />
        {isEdit && (
          <p className="text-xs text-muted-foreground">分组 ID 不可修改</p>
        )}
      </div>

      <div className="space-y-1.5">
        <Label className="text-sm">代理 URL</Label>
        <Input
          placeholder="socks5://host:port（留空 = 直连）"
          value={proxyUrl}
          onChange={(e) => setProxyUrl(e.target.value)}
        />
        <p className="text-xs text-muted-foreground leading-relaxed">
          支持 <code>socks5://</code> / <code>http://</code>。留空或填 <code>direct</code> 表示该组直连，
          且忽略全局代理。
        </p>
      </div>

      <div className="grid grid-cols-2 gap-3">
        <div className="space-y-1.5">
          <Label className="text-sm">代理用户名</Label>
          <Input
            placeholder="可选"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
          />
        </div>
        <div className="space-y-1.5">
          <Label className="text-sm">代理密码</Label>
          <Input
            type="password"
            placeholder={
              isEdit && existing?.hasUsername ? '留空 = 保留原密码' : '可选'
            }
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
        </div>
      </div>
      {isEdit && (
        <p className="-mt-2 text-xs text-muted-foreground">
          出于安全，密码不回显。留空表示保留原密码；要清空请改成直连或重建分组。
        </p>
      )}

      <DialogFooter className="gap-2 sm:gap-2">
        <Button variant="ghost" onClick={onBack} disabled={upsert.isPending}>
          <ArrowLeft className="h-4 w-4 mr-1" />
          返回
        </Button>
        <Button onClick={handleSave} disabled={upsert.isPending}>
          {upsert.isPending ? '保存中…' : isEdit ? '保存修改' : '创建分组'}
        </Button>
      </DialogFooter>
    </div>
  )
}


