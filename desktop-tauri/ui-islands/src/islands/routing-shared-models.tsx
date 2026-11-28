import * as React from 'react'
import { Button, Empty } from '@ui'
import { t } from '../i18n'
import { bridge, errorMessage, notify } from './routing-types'

/** 一个跨厂同名模型的发现结果（与后端 `discover_shared_models` 同形）。 */
export type SharedModelMember = {
  provider: string
  providerLabel: string
  model: string
  accountCount: number
}

export type SharedModel = {
  /** 对外模型名（用第一家承载家的原样名字） */
  model: string
  /** 归并键（小写），同键即同名 */
  key: string
  /** 承载它的提供商 id 列表 */
  providers: string[]
  /** 各家成员（上游名 + 账号数） */
  members: SharedModelMember[]
}

/**
 * 跨厂同名模型的「发现面板」。
 *
 * 转发层**本来就**跨厂容灾：客户端点一个模型名，`route_for_forward` 会把
 * 承载该模型的各家账号汇进同一条全局队列，A 家失败自动顺延到 B 家。这个
 * 面板做的是把这件事**显式化** —— 让用户看得见「这个模型有几家在顶」，
 * 并且愿意的话一键把这些名字绑成一个路由组（组内成员可再各自配顺序与策略）。
 *
 * 只读 + 一键建组，**不改变**已生效的转发行为：没有组的模型照样跨厂容灾。
 */
export function RoutingSharedModels({ onCreated }: { onCreated?: () => void }) {
  const [shared, setShared] = React.useState<SharedModel[]>([])
  const [loading, setLoading] = React.useState(false)
  const [error, setError] = React.useState('')
  const [creating, setCreating] = React.useState('')

  const load = React.useCallback(async () => {
    setLoading(true); setError('')
    try {
      const result = await bridge().getRoutingSharedModels()
      setShared(Array.isArray(result?.shared) ? result.shared : [])
    } catch (failure) {
      setError(errorMessage(failure))
    } finally {
      setLoading(false)
    }
  }, [])

  React.useEffect(() => { void load() }, [load])

  /** 一键把同名模型建成一个路由组：成员 = 各家承载该模型的提供商。 */
  const createGroup = async (entry: SharedModel) => {
    if (creating) return
    setCreating(entry.key); setError('')
    try {
      const groups = await bridge().getRoutingGroups()
      // 组 id 用模型名本身（同名模型本来就是一个对外名，重名即同一组）
      const id = entry.model
      if (groups.groups.some(row => row.group.id === id)) {
        notify(t('该模型已经有一个路由组了'), 'err')
        return
      }
      await bridge().saveRoutingGroup({
        id,
        group: {
          id,
          name: entry.model,
          model: entry.model,
          description: t('自动同名组：{model} 由 {count} 家提供商共同承载',
            { model: entry.model, count: entry.providers.length }),
          strategy: 'order',
          sticky: 'turn',
          members: entry.members.map(member => ({
            id: member.provider,
            provider: member.provider,
            model: member.model,
            included: true,
            accounts: [],
          })),
        },
        revision: groups.newRevision,
      })
      notify(t('已创建同名组：{model}', { model: entry.model }))
      onCreated?.()
    } catch (failure) {
      setError(errorMessage(failure))
    } finally {
      setCreating('')
    }
  }

  if (loading && shared.length === 0) return <Empty>{t('正在查找跨厂同名模型…')}</Empty>
  if (shared.length === 0) {
    return <Empty>{error ? error : t('当前没有跨厂同名模型（同一个模型名只被一家承载）')}</Empty>
  }

  return (
    <div className='routing-shared-models'>
      <p className='detail'>
        {t('这些模型名被多家提供商同时承载：客户端点名它们时，网关会在这几家的账号之间自动顺延。也可以一键建成路由组，手动指定顺序与策略。')}
      </p>
      {error ? <p className='text-destructive'>{error}</p> : null}
      {shared.map(entry => (
        <div key={entry.key} className='routing-shared-model'>
          <div className='routing-shared-model-head'>
            <strong>{entry.model}</strong>
            <Button size='xs' variant='outline' disabled={!!creating}
              onClick={() => void createGroup(entry)}>
              {creating === entry.key ? t('创建中…') : t('一键建组')}
            </Button>
          </div>
          <ul className='routing-shared-model-members'>
            {entry.members.map(member => (
              <li key={member.provider}>
                <span>{member.providerLabel || member.provider}</span>
                <span className='detail'>{member.model}</span>
                <span className='detail'>{t('{count} 个账号', { count: member.accountCount })}</span>
              </li>
            ))}
          </ul>
        </div>
      ))}
    </div>
  )
}
