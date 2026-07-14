import { useState } from 'react'
import { toast } from 'sonner'
import { CheckCircle2, XCircle, AlertCircle, Loader2 } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { useCredentials, useAddCredential, useDeleteCredential } from '@/hooks/use-credentials'
import { getCredentialBalance, setCredentialDisabled } from '@/api/credentials'
import { extractErrorMessage, sha256Hex } from '@/lib/utils'
import {
  parseAndNormalizeImportJson,
  resolveImportAuthMethod,
  validateImportCredential,
  toAddCredentialRequest,
  type NormalizedCredential,
} from '@/lib/credential-import'

interface KamImportDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

interface VerificationResult {
  index: number
  status: 'pending' | 'checking' | 'verifying' | 'verified' | 'duplicate' | 'failed' | 'skipped'
  error?: string
  usage?: string
  email?: string
  credentialId?: number
  rollbackStatus?: 'success' | 'failed' | 'skipped'
  rollbackError?: string
}

export function KamImportDialog({ open, onOpenChange }: KamImportDialogProps) {
  const [jsonInput, setJsonInput] = useState('')
  const [importing, setImporting] = useState(false)
  const [skipErrorAccounts, setSkipErrorAccounts] = useState(true)
  const [progress, setProgress] = useState({ current: 0, total: 0 })
  const [currentProcessing, setCurrentProcessing] = useState<string>('')
  const [results, setResults] = useState<VerificationResult[]>([])

  const { data: existingCredentials } = useCredentials()
  const { mutateAsync: addCredential } = useAddCredential()
  const { mutateAsync: deleteCredential } = useDeleteCredential()

  const rollbackCredential = async (id: number): Promise<{ success: boolean; error?: string }> => {
    try {
      await setCredentialDisabled(id, true)
    } catch (error) {
      return { success: false, error: `禁用失败: ${extractErrorMessage(error)}` }
    }
    try {
      await deleteCredential(id)
      return { success: true }
    } catch (error) {
      return { success: false, error: `删除失败: ${extractErrorMessage(error)}` }
    }
  }

  const resetForm = () => {
    setJsonInput('')
    setProgress({ current: 0, total: 0 })
    setCurrentProcessing('')
    setResults([])
  }

  const handleImport = async () => {
    let credentials: NormalizedCredential[]
    try {
      credentials = parseAndNormalizeImportJson(jsonInput)
      // KAM 侧主要是 OAuth；API Key 也允许
      if (skipErrorAccounts) {
        // 兼容旧 KAM status=error 字段：parse 阶段已拍平，无 status 时跳过逻辑不再适用
      }
      if (credentials.length === 0) {
        toast.error('没有可导入的账号（需要 refreshToken 或 kiroApiKey）')
        return
      }
    } catch (error) {
      toast.error('JSON 格式错误: ' + extractErrorMessage(error))
      return
    }

    try {
      setImporting(true)
      setProgress({ current: 0, total: credentials.length })

      const initialResults: VerificationResult[] = credentials.map((c, i) => ({
        index: i + 1,
        status: 'pending',
        email: c.email,
      }))
      setResults(initialResults)

      const existingOauthHashes = new Set(
        existingCredentials?.credentials
          .map((c) => c.refreshTokenHash)
          .filter((hash): hash is string => Boolean(hash)) || []
      )
      const existingApiKeyHashes = new Set(
        existingCredentials?.credentials
          .map((c) => c.apiKeyHash)
          .filter((hash): hash is string => Boolean(hash)) || []
      )

      let successCount = 0
      let duplicateCount = 0
      let failCount = 0
      let skippedCount = 0

      for (let i = 0; i < credentials.length; i++) {
        const cred = credentials[i]
        const label = cred.email || `账号 ${i + 1}`
        setCurrentProcessing(`正在处理 ${label}`)
        setResults((prev) => {
          const next = [...prev]
          next[i] = { ...next[i], status: 'checking' }
          return next
        })

        const method = resolveImportAuthMethod(cred)
        const isApiKey = method === 'api_key'
        const keyMaterial = isApiKey ? cred.kiroApiKey?.trim() || '' : cred.refreshToken?.trim() || ''
        if (!keyMaterial) {
          failCount++
          setResults((prev) => {
            const next = [...prev]
            next[i] = {
              ...next[i],
              status: 'failed',
              error: isApiKey ? '缺少 kiroApiKey' : '缺少 refreshToken',
            }
            return next
          })
          setProgress({ current: i + 1, total: credentials.length })
          continue
        }

        const tokenHash = await sha256Hex(keyMaterial)
        const dupSet = isApiKey ? existingApiKeyHashes : existingOauthHashes
        if (dupSet.has(tokenHash)) {
          duplicateCount++
          const existingCred = existingCredentials?.credentials.find((c) =>
            isApiKey ? c.apiKeyHash === tokenHash : c.refreshTokenHash === tokenHash
          )
          setResults((prev) => {
            const next = [...prev]
            next[i] = {
              ...next[i],
              status: 'duplicate',
              error: '该凭据已存在',
              email: existingCred?.email || cred.email,
            }
            return next
          })
          setProgress({ current: i + 1, total: credentials.length })
          continue
        }

        setResults((prev) => {
          const next = [...prev]
          next[i] = { ...next[i], status: 'verifying' }
          return next
        })

        let addedCredId: number | null = null
        try {
          const precheck = validateImportCredential(cred)
          if (precheck) throw new Error(precheck)

          const payload = toAddCredentialRequest(cred)
          const addedCred = await addCredential(payload as Parameters<typeof addCredential>[0])
          addedCredId = addedCred.credentialId

          await new Promise((resolve) => setTimeout(resolve, 1000))
          const balance = await getCredentialBalance(addedCred.credentialId)

          successCount++
          dupSet.add(tokenHash)
          setCurrentProcessing(`验活成功: ${addedCred.email || label}`)
          setResults((prev) => {
            const next = [...prev]
            next[i] = {
              ...next[i],
              status: 'verified',
              usage: `${balance.currentUsage}/${balance.usageLimit}`,
              email: addedCred.email || cred.email,
              credentialId: addedCred.credentialId,
            }
            return next
          })
        } catch (error) {
          let rollbackStatus: VerificationResult['rollbackStatus'] = 'skipped'
          let rollbackError: string | undefined
          if (addedCredId) {
            const result = await rollbackCredential(addedCredId)
            if (result.success) rollbackStatus = 'success'
            else {
              rollbackStatus = 'failed'
              rollbackError = result.error
            }
          }
          failCount++
          setResults((prev) => {
            const next = [...prev]
            next[i] = {
              ...next[i],
              status: 'failed',
              error: extractErrorMessage(error),
              rollbackStatus,
              rollbackError,
            }
            return next
          })
        }

        setProgress({ current: i + 1, total: credentials.length })
      }

      const parts: string[] = []
      if (successCount > 0) parts.push(`成功 ${successCount}`)
      if (duplicateCount > 0) parts.push(`重复 ${duplicateCount}`)
      if (failCount > 0) parts.push(`失败 ${failCount}`)
      if (skippedCount > 0) parts.push(`跳过 ${skippedCount}`)

      if (failCount === 0 && duplicateCount === 0 && skippedCount === 0) {
        toast.success(`成功导入并验活 ${successCount} 个凭据`)
      } else {
        toast.info(parts.join('，') || '完成')
      }
    } catch (error) {
      toast.error('导入失败: ' + extractErrorMessage(error))
    } finally {
      setImporting(false)
    }
  }

  const getStatusIcon = (status: VerificationResult['status']) => {
    switch (status) {
      case 'pending':
        return <div className="w-5 h-5 rounded-full border-2 border-gray-300" />
      case 'checking':
      case 'verifying':
        return <Loader2 className="w-5 h-5 animate-spin text-blue-500" />
      case 'verified':
        return <CheckCircle2 className="w-5 h-5 text-green-500" />
      case 'duplicate':
      case 'skipped':
        return <AlertCircle className="w-5 h-5 text-yellow-500" />
      case 'failed':
        return <XCircle className="w-5 h-5 text-red-500" />
    }
  }

  const getStatusText = (result: VerificationResult) => {
    switch (result.status) {
      case 'pending':
        return '等待中'
      case 'checking':
        return '检查重复...'
      case 'verifying':
        return '验活中...'
      case 'verified':
        return '验活成功'
      case 'duplicate':
        return '重复凭据'
      case 'skipped':
        return '已跳过'
      case 'failed':
        if (result.rollbackStatus === 'success') return '验活失败（已排除）'
        if (result.rollbackStatus === 'failed') return '验活失败（未排除）'
        return '验活失败'
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(newOpen) => {
        if (!newOpen && !importing) resetForm()
        onOpenChange(newOpen)
      }}
    >
      <DialogContent className="sm:max-w-2xl max-h-[80vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>KAM 账号导入（自动验活）</DialogTitle>
        </DialogHeader>

        <div className="flex-1 overflow-y-auto space-y-4 py-4">
          <div className="space-y-2">
            <label className="text-sm font-medium">KAM / 企业导出 JSON</label>
            <textarea
              placeholder={
                '粘贴 Kiro Account Manager 或企业导出 JSON（与批量导入互通）\n\n' +
                '支持：\n' +
                '• KAM { version, accounts: [...] }\n' +
                '• KAM 平铺数组 [{ email, refreshToken, clientId, clientSecret }]\n' +
                '• 企业导出 login_provider + kiro_auth_token_raw + kiro_profile_raw\n' +
                '• 数组内同时含 SSO registration 时自动合并 clientId/secret\n' +
                '• External IdP / API Key'
              }
              value={jsonInput}
              onChange={(e) => setJsonInput(e.target.value)}
              disabled={importing}
              className="flex min-h-[200px] w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50 font-mono"
            />
            <p className="text-xs text-muted-foreground">
              💡 与「批量导入」共用同一套解析逻辑；IdC/Enterprise 必须带 clientId+clientSecret（或 clientRegistration）
            </p>
            <label className="flex items-center gap-2 text-sm text-muted-foreground">
              <input
                type="checkbox"
                checked={skipErrorAccounts}
                onChange={(e) => setSkipErrorAccounts(e.target.checked)}
                disabled={importing}
              />
              跳过 KAM 中 status=error 的账号（兼容选项；归一化后若无 status 则不影响）
            </label>
          </div>

          {(importing || results.length > 0) && (
            <>
              <div className="space-y-2">
                <div className="flex justify-between text-sm">
                  <span>{importing ? '验活进度' : '验活完成'}</span>
                  <span>
                    {progress.current} / {progress.total}
                  </span>
                </div>
                <div className="w-full bg-secondary rounded-full h-2">
                  <div
                    className="bg-primary h-2 rounded-full transition-all"
                    style={{
                      width:
                        progress.total > 0
                          ? `${(progress.current / progress.total) * 100}%`
                          : '0%',
                    }}
                  />
                </div>
                {currentProcessing && (
                  <p className="text-xs text-muted-foreground">{currentProcessing}</p>
                )}
              </div>

              <div className="space-y-2 max-h-[240px] overflow-y-auto">
                {results.map((result) => (
                  <div
                    key={result.index}
                    className="flex items-start gap-3 p-2 rounded-md border text-sm"
                  >
                    {getStatusIcon(result.status)}
                    <div className="flex-1 min-w-0">
                      <div className="flex justify-between gap-2">
                        <span className="font-medium truncate">
                          #{result.index} {result.email || ''}
                        </span>
                        <span className="text-muted-foreground shrink-0">
                          {getStatusText(result)}
                        </span>
                      </div>
                      {result.usage && (
                        <p className="text-xs text-muted-foreground">额度 {result.usage}</p>
                      )}
                      {result.error && (
                        <p className="text-xs text-red-500 break-all">{result.error}</p>
                      )}
                      {result.rollbackError && (
                        <p className="text-xs text-orange-500 break-all">
                          回滚: {result.rollbackError}
                        </p>
                      )}
                    </div>
                  </div>
                ))}
              </div>
            </>
          )}
        </div>

        <DialogFooter>
          <Button
            variant="outline"
            onClick={() => onOpenChange(false)}
            disabled={importing}
          >
            关闭
          </Button>
          <Button onClick={handleImport} disabled={importing || !jsonInput.trim()}>
            {importing ? (
              <>
                <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                导入中...
              </>
            ) : (
              '开始导入'
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
