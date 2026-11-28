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
  /**
   * 建议的**路由组对外 ID**（后端给的）。
   *
   * 不能直接用 `model` 当对外 ID —— 后端有一条硬规则：对外 ID 若已被任何
   * 提供商承载就拒绝建组（否则客户端点这个名字时无法区分走路由组还是走
   * 原生模型）。同名模型必然被多家承载，所以要加一个后缀。
   */
  suggestedModel?: string
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
export function RoutingSharedModels({ onCreated, refreshToken }: { onCreated?: () => void; refreshToken?: number }) {
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

  // `refreshToken` 变化时重新拉取（页面上的「重新读取」会递增它）
  React.useEffect(() => { void load() }, [load, refreshToken])

  /**
   * 一键把同名模型建成一个路由组：成员 = 各家承载该模型的提供商。
   *
   * ⚠️ 对外模型 ID **不能**直接用原模型名 —— 后端 `routing_groups::save` 有
   * 一条硬规则：对外 ID 若已被任何提供商承载就拒绝建组（否则客户端点这个名字
   * 时无法区分走路由组还是走原生模型）。同名模型必然被多家承载，所以用后端
   * 建议的名字（原名 + `-auto` 后缀），成员仍是各家的上游模型名。
   */
  const createGroup = async (entry: SharedModel) => {
    if (creating) return
    setCreating(entry.key); setError('')
    try {
      const groups = await bridge().getRoutingGroups()
      // 对外 ID 由后端建议（规则由后端校验决定，这里不自己拼）
      const id = entry.suggestedModel || `${entry.model}-auto`
      await bridge().saveRoutingGroup({
        id,
        group: {
          id,
          name: entry.model,
          model: id,
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
      notify(t('已创建同名组：{model}（对外模型名 {id}）', { model: entry.model, id }), 'ok')
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
          {entry.suggestedModel ? (
            <p className='detail' style={{ margin: '0 0 8px' }}>
              {t('建组后的对外模型名：{id}（客户端点这个名字才会走这个组）', { id: entry.suggestedModel })}
            </p>
          ) : null}
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
