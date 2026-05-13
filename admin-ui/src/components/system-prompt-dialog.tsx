import { useEffect, useMemo, useState } from 'react'
import {
  ChevronDown,
  ChevronRight,
  Eye,
  Loader2,
  Pencil,
  Plus,
  RotateCcw,
  Save,
  Sparkles,
  Trash2,
  X,
} from 'lucide-react'
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
import { Switch } from '@/components/ui/switch'
import { Textarea } from '@/components/ui/textarea'
import { Label } from '@/components/ui/label'
import { Input } from '@/components/ui/input'
import {
  useAddUserPreset,
  useDeleteUserPreset,
  usePresetCatalog,
  useSystemPrompt,
  useUpdateSystemPrompt,
  useUpdateUserPreset,
} from '@/hooks/use-system-prompt'
import type {
  PresetMeta,
  SystemPromptPosition,
  UserPreset,
} from '@/types/api'
import { extractErrorMessage } from '@/lib/utils'

interface SystemPromptDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function SystemPromptDialog({ open, onOpenChange }: SystemPromptDialogProps) {
  const { data, isLoading, error, refetch } = useSystemPrompt()
  const { data: catalog, isLoading: loadingCatalog } = usePresetCatalog()
  const { mutateAsync: update, isPending: saving } = useUpdateSystemPrompt()
  const { mutateAsync: addPreset, isPending: addingPreset } = useAddUserPreset()
  const { mutateAsync: updatePreset } = useUpdateUserPreset()
  const { mutateAsync: deletePreset } = useDeleteUserPreset()

  // ── 本地表单状态 ──
  const [enabled, setEnabled] = useState(false)
  const [enabledPresets, setEnabledPresets] = useState<Set<string>>(new Set())
  const [content, setContent] = useState('')
  const [position, setPosition] = useState<SystemPromptPosition>('append')
  const [stripRestrictions, setStripRestrictions] = useState(false)
  const [dirty, setDirty] = useState(false)

  // 预览展开/收起
  const [previewExpanded, setPreviewExpanded] = useState<string | null>(null) // preset id
  const [finalPreviewOpen, setFinalPreviewOpen] = useState(false)

  // 用户预设编辑器状态
  const [editorOpen, setEditorOpen] = useState(false)
  const [editingId, setEditingId] = useState<string | null>(null) // null = 新增
  const [formId, setFormId] = useState('')
  const [formName, setFormName] = useState('')
  const [formDesc, setFormDesc] = useState('')
  const [formContent, setFormContent] = useState('')
  const [savingPreset, setSavingPreset] = useState(false)

  // 服务端数据回填
  useEffect(() => {
    if (!data || dirty) return
    setEnabled(data.enabled)
    setEnabledPresets(new Set(data.enabledPresets))
    setContent(data.content ?? '')
    setPosition(data.position)
    setStripRestrictions(data.stripRestrictions)
  }, [data, dirty])

  // 打开/关闭管理
  useEffect(() => {
    if (!open) {
      setDirty(false)
      setPreviewExpanded(null)
      setFinalPreviewOpen(false)
      setEditorOpen(false)
    } else {
      refetch()
    }
  }, [open, refetch])

  const togglePreset = (id: string) => {
    setEnabledPresets((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
    setDirty(true)
  }

  const handleSave = async () => {
    try {
      await update({
        enabled,
        enabledPresets: Array.from(enabledPresets),
        content,
        position,
        stripRestrictions,
      })
      setDirty(false)
      toast.success('系统提示词配置已保存并即时生效')
    } catch (e) {
      toast.error(`保存失败: ${extractErrorMessage(e)}`)
    }
  }

  // ── 用户预设编辑器 ──
  const openAddPreset = () => {
    setEditingId(null)
    setFormId('')
    setFormName('')
    setFormDesc('')
    setFormContent('')
    setEditorOpen(true)
  }

  const openEditPreset = (p: UserPreset) => {
    setEditingId(p.id)
    setFormId(p.id)
    setFormName(p.name)
    setFormDesc(p.description)
    setFormContent(p.content)
    setEditorOpen(true)
  }

  const handleSubmitPreset = async () => {
    if (!formName.trim() || !formContent.trim()) {
      toast.error('名称和内容不能为空')
      return
    }
    if (!editingId && !/^[a-z0-9_-]{1,32}$/.test(formId)) {
      toast.error('id 必须为 1-32 个小写字母/数字/下划线/短横线')
      return
    }
    setSavingPreset(true)
    try {
      if (editingId) {
        await updatePreset({
          id: editingId,
          req: { name: formName, description: formDesc, content: formContent },
        })
        toast.success(`已更新预设 ${editingId}`)
      } else {
        await addPreset({
          id: formId,
          name: formName,
          description: formDesc,
          content: formContent,
        })
        toast.success(`已添加预设 ${formId}`)
      }
      setEditorOpen(false)
    } catch (e) {
      toast.error(`保存失败: ${extractErrorMessage(e)}`)
    } finally {
      setSavingPreset(false)
    }
  }

  const handleDeletePreset = async (id: string) => {
    if (!confirm(`确定删除自定义预设 "${id}"？`)) return
    try {
      await deletePreset(id)
      // 同步本地 enabledPresets 状态
      setEnabledPresets((prev) => {
        if (!prev.has(id)) return prev
        const next = new Set(prev)
        next.delete(id)
        return next
      })
      toast.success(`已删除预设 ${id}`)
    } catch (e) {
      toast.error(`删除失败: ${extractErrorMessage(e)}`)
    }
  }

  // ── 拼合所有 preset（内置 + 用户）用于渲染 ──
  const allPresets = useMemo(() => {
    if (!catalog || !data) return [] as Array<PresetMeta & { builtin: boolean }>
    const builtin = catalog.presets.map((p) => ({ ...p, builtin: true }))
    const user = data.userPresets.map((p) => ({
      id: p.id,
      name: p.name,
      description: p.description,
      length: p.content.length,
      content: p.content,
      builtin: false,
    }))
    return [...builtin, ...user]
  }, [catalog, data])

  // ── B：本地拼接「最终注入」预览文本 ──
  const previewText = useMemo(() => {
    if (!enabled) return ''
    const parts: string[] = []
    // 1. 内置 preset (catalog 顺序)
    catalog?.presets.forEach((p) => {
      if (enabledPresets.has(p.id)) parts.push(p.content.trim())
    })
    // 2. 用户 preset (data.userPresets 顺序)
    data?.userPresets.forEach((p) => {
      if (enabledPresets.has(p.id)) {
        const t = p.content.trim()
        if (t) parts.push(t)
      }
    })
    // 3. custom
    const c = content.trim()
    if (c) parts.push(c)
    return parts.join('\n\n')
  }, [enabled, enabledPresets, content, catalog, data])

  const activeCount = enabledPresets.size + (content.trim().length > 0 ? 1 : 0)

  return (
    <>
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent className="sm:max-w-3xl max-h-[92vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <Sparkles className="h-5 w-5" />
              系统提示词管理
            </DialogTitle>
            <DialogDescription>
              选择启用的预设 + 自定义补充，按位置注入到每次请求的 system role。
              保存后即时生效，自动写回 <code>config.json</code>。
            </DialogDescription>
          </DialogHeader>

          {(isLoading || loadingCatalog) && (
            <div className="flex items-center justify-center py-10">
              <Loader2 className="h-6 w-6 animate-spin text-muted-foreground" />
            </div>
          )}

          {error && (
            <div className="py-4 text-sm text-red-500">
              加载失败: {extractErrorMessage(error)}
            </div>
          )}

          {data && catalog && !isLoading && (
            <div className="space-y-5">
              {/* ── 总开关 ── */}
              <div className="flex items-center justify-between gap-4 rounded-lg border-2 border-primary/30 bg-primary/5 p-4">
                <div className="space-y-0.5">
                  <Label htmlFor="master-switch" className="text-base font-semibold">
                    注入总开关
                  </Label>
                  <p className="text-xs text-muted-foreground">
                    关闭后所有 preset 和自定义文本都不会注入（剥离开关独立工作）
                  </p>
                </div>
                <Switch
                  id="master-switch"
                  checked={enabled}
                  onCheckedChange={(v) => {
                    setEnabled(v)
                    setDirty(true)
                  }}
                />
              </div>

              {/* ── 内置 + 用户预设清单 ── */}
              <div className="space-y-2">
                <div className="flex items-center justify-between">
                  <Label className="text-sm font-medium">
                    预设清单
                    <span className="ml-2 text-xs text-muted-foreground">
                      （{enabledPresets.size}/{allPresets.length} 启用 ·{' '}
                      {catalog.presets.length} 内置 + {data.userPresets.length} 自定义）
                    </span>
                  </Label>
                  <Button
                    type="button"
                    size="sm"
                    variant="outline"
                    onClick={openAddPreset}
                  >
                    <Plus className="h-3.5 w-3.5 mr-1" />
                    新增自定义
                  </Button>
                </div>

                <div className="space-y-2">
                  {allPresets.map((preset) => {
                    const checked = enabledPresets.has(preset.id)
                    const expanded = previewExpanded === preset.id
                    return (
                      <div
                        key={preset.id}
                        className={`rounded-lg border transition-colors ${
                          checked ? 'border-primary/50 bg-primary/[0.03]' : 'border-input'
                        }`}
                      >
                        <div className="flex items-start gap-3 p-3">
                          <Switch
                            checked={checked}
                            onCheckedChange={() => togglePreset(preset.id)}
                            disabled={!enabled}
                            className="mt-0.5"
                          />
                          <div className="flex-1 min-w-0">
                            <div className="flex items-center gap-2 flex-wrap">
                              <span className="text-sm font-medium">{preset.name}</span>
                              <code className="text-[10px] px-1.5 py-0.5 rounded bg-muted text-muted-foreground">
                                {preset.id}
                              </code>
                              {preset.builtin ? (
                                <span className="text-[10px] px-1.5 py-0.5 rounded bg-blue-500/10 text-blue-600 dark:text-blue-400">
                                  内置
                                </span>
                              ) : (
                                <span className="text-[10px] px-1.5 py-0.5 rounded bg-purple-500/10 text-purple-600 dark:text-purple-400">
                                  自定义
                                </span>
                              )}
                              <span className="text-[10px] text-muted-foreground">
                                {preset.length} chars
                              </span>
                            </div>
                            {preset.description && (
                              <p className="text-xs text-muted-foreground mt-1">
                                {preset.description}
                              </p>
                            )}
                          </div>
                          <div className="flex gap-1">
                            <Button
                              type="button"
                              size="sm"
                              variant="ghost"
                              onClick={() =>
                                setPreviewExpanded(expanded ? null : preset.id)
                              }
                              title="预览完整内容"
                            >
                              <Eye className="h-3.5 w-3.5" />
                            </Button>
                            {!preset.builtin && (
                              <>
                                <Button
                                  type="button"
                                  size="sm"
                                  variant="ghost"
                                  onClick={() =>
                                    openEditPreset(
                                      data.userPresets.find((p) => p.id === preset.id)!
                                    )
                                  }
                                  title="编辑"
                                >
                                  <Pencil className="h-3.5 w-3.5" />
                                </Button>
                                <Button
                                  type="button"
                                  size="sm"
                                  variant="ghost"
                                  onClick={() => handleDeletePreset(preset.id)}
                                  title="删除"
                                  className="text-destructive hover:text-destructive"
                                >
                                  <Trash2 className="h-3.5 w-3.5" />
                                </Button>
                              </>
                            )}
                          </div>
                        </div>
                        {expanded && (
                          <pre className="mx-3 mb-3 max-h-48 overflow-y-auto rounded bg-muted/60 p-2 text-[11px] leading-relaxed whitespace-pre-wrap break-words">
                            {preset.content}
                          </pre>
                        )}
                      </div>
                    )
                  })}
                </div>
              </div>

              {/* ── 自定义补充 ── */}
              <div className="space-y-2">
                <div className="flex items-center justify-between">
                  <Label htmlFor="custom-content">
                    自定义补充文本
                    <span className="ml-2 text-xs text-muted-foreground">
                      （{content.trim().length} 字符，追加在所有 preset 之后）
                    </span>
                  </Label>
                  <Button
                    type="button"
                    size="sm"
                    variant="ghost"
                    onClick={() => {
                      setContent('')
                      setDirty(true)
                    }}
                    disabled={content.length === 0}
                  >
                    <RotateCcw className="h-3.5 w-3.5 mr-1" />
                    清空
                  </Button>
                </div>
                <Textarea
                  id="custom-content"
                  value={content}
                  onChange={(e) => {
                    setContent(e.target.value)
                    setDirty(true)
                  }}
                  placeholder="可选 — 在所有启用的 preset 之后追加你自己的补充指令"
                  className="min-h-[120px] text-xs"
                  spellCheck={false}
                  disabled={!enabled}
                />
              </div>

              {/* ── B：最终注入预览 ── */}
              <div className="rounded-lg border">
                <button
                  type="button"
                  onClick={() => setFinalPreviewOpen(!finalPreviewOpen)}
                  className="flex w-full items-center justify-between p-3 text-sm font-medium hover:bg-accent/50 transition-colors"
                  disabled={!enabled}
                >
                  <span className="flex items-center gap-2">
                    {finalPreviewOpen ? (
                      <ChevronDown className="h-4 w-4" />
                    ) : (
                      <ChevronRight className="h-4 w-4" />
                    )}
                    最终注入预览（{previewText.length} 字符 · {activeCount} 项生效）
                  </span>
                  {!enabled && (
                    <span className="text-xs text-muted-foreground">
                      总开关已关闭
                    </span>
                  )}
                </button>
                {finalPreviewOpen && enabled && (
                  <div className="border-t p-3">
                    {previewText.length === 0 ? (
                      <p className="text-xs text-muted-foreground italic">
                        当前配置不会注入任何内容 — 至少启用一个 preset 或填写自定义文本
                      </p>
                    ) : (
                      <pre className="max-h-[400px] overflow-y-auto rounded bg-muted/40 p-3 text-[11px] leading-relaxed whitespace-pre-wrap break-words">
                        {previewText}
                      </pre>
                    )}
                  </div>
                )}
              </div>

              {/* ── 注入位置 ── */}
              <div className="space-y-2">
                <Label>注入位置</Label>
                <div className="flex gap-2">
                  <PositionButton
                    active={position === 'append'}
                    onClick={() => {
                      setPosition('append')
                      setDirty(true)
                    }}
                    disabled={!enabled}
                    title="append (推荐)"
                    desc="放到 system 末尾 - recency bias 权重最高"
                  />
                  <PositionButton
                    active={position === 'prepend'}
                    onClick={() => {
                      setPosition('prepend')
                      setDirty(true)
                    }}
                    disabled={!enabled}
                    title="prepend"
                    desc="放到 system 最前 - 易被后续指令覆盖"
                  />
                </div>
              </div>

              {/* ── 剥离限制（独立开关）── */}
              <div className="flex items-start justify-between gap-4 rounded-lg border p-4">
                <div className="space-y-1">
                  <Label htmlFor="strip-switch" className="text-sm font-medium">
                    剥离客户端安全限制
                  </Label>
                  <p className="text-xs text-muted-foreground">
                    独立开关，与上面注入总开关无关。开启后会移除 Claude Code 内置的安全测试拒绝、
                    Git Safety、Sandbox 等约束（等价 patch #1–#20）。
                  </p>
                </div>
                <Switch
                  id="strip-switch"
                  checked={stripRestrictions}
                  onCheckedChange={(v) => {
                    setStripRestrictions(v)
                    setDirty(true)
                  }}
                />
              </div>
            </div>
          )}

          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => onOpenChange(false)}
              disabled={saving}
            >
              取消
            </Button>
            <Button onClick={handleSave} disabled={saving || !dirty || isLoading}>
              {saving ? (
                <Loader2 className="h-4 w-4 mr-2 animate-spin" />
              ) : (
                <Save className="h-4 w-4 mr-2" />
              )}
              {dirty ? '保存' : '已保存'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* ── C：用户预设编辑器子对话框 ── */}
      <Dialog open={editorOpen} onOpenChange={setEditorOpen}>
        <DialogContent className="sm:max-w-xl">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              {editingId ? (
                <>
                  <Pencil className="h-4 w-4" />
                  编辑自定义预设
                </>
              ) : (
                <>
                  <Plus className="h-4 w-4" />
                  新增自定义预设
                </>
              )}
            </DialogTitle>
            <DialogDescription>
              {editingId
                ? '修改名称、描述、内容（id 不可改）'
                : 'id 用作配置文件中的标识，保存后不可修改'}
            </DialogDescription>
          </DialogHeader>

          <div className="space-y-4">
            <div className="space-y-1.5">
              <Label htmlFor="form-id">
                id <span className="text-muted-foreground">(1-32 小写字母/数字/_/-)</span>
              </Label>
              <Input
                id="form-id"
                value={formId}
                onChange={(e) => setFormId(e.target.value)}
                placeholder="my_preset"
                disabled={!!editingId || addingPreset || savingPreset}
              />
            </div>

            <div className="space-y-1.5">
              <Label htmlFor="form-name">名称</Label>
              <Input
                id="form-name"
                value={formName}
                onChange={(e) => setFormName(e.target.value)}
                placeholder="我的预设"
                disabled={savingPreset}
              />
            </div>

            <div className="space-y-1.5">
              <Label htmlFor="form-desc">描述（可选）</Label>
              <Input
                id="form-desc"
                value={formDesc}
                onChange={(e) => setFormDesc(e.target.value)}
                placeholder="一句话说明这个预设的用途"
                disabled={savingPreset}
              />
            </div>

            <div className="space-y-1.5">
              <Label htmlFor="form-content">
                内容
                <span className="ml-2 text-xs text-muted-foreground">
                  ({formContent.length} 字符)
                </span>
              </Label>
              <Textarea
                id="form-content"
                value={formContent}
                onChange={(e) => setFormContent(e.target.value)}
                placeholder="完整的 prompt 文本"
                className="min-h-[180px] text-xs"
                spellCheck={false}
                disabled={savingPreset}
              />
            </div>
          </div>

          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setEditorOpen(false)}
              disabled={savingPreset}
            >
              <X className="h-4 w-4 mr-1" />
              取消
            </Button>
            <Button onClick={handleSubmitPreset} disabled={savingPreset}>
              {savingPreset ? (
                <Loader2 className="h-4 w-4 mr-2 animate-spin" />
              ) : (
                <Save className="h-4 w-4 mr-2" />
              )}
              保存
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}

interface PositionButtonProps {
  active: boolean
  onClick: () => void
  disabled?: boolean
  title: string
  desc: string
}

function PositionButton({ active, onClick, disabled, title, desc }: PositionButtonProps) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className={`flex-1 rounded-md border p-3 text-left transition-colors disabled:opacity-50 disabled:cursor-not-allowed ${
        active
          ? 'border-primary bg-primary/5 ring-1 ring-primary'
          : 'border-input hover:bg-accent'
      }`}
    >
      <div className="text-sm font-medium">{title}</div>
      <div className="text-xs text-muted-foreground mt-0.5">{desc}</div>
    </button>
  )
}
