import * as React from 'react'
import { Button, Input, Label } from '@ui'
import { t } from '../i18n'
import { RoutingSelect } from './routing-select'
import { notify, type RouteGroup, type RouteMember } from './routing-types'

/** 一个意图档位（与后端 `IntentLevel` 同形）。 */
export type IntentLevel = {
  /** 档位标识 */
  id: string
  /** 给分类器看的档位描述（一句话说明「什么样的请求属于这一档」） */
  label: string
  /** 命中这一档时保留的成员 id 列表 */
  members: string[]
}

/** 意图分类配置（与后端 `IntentClassifier` 同形）。 */
export type RouteIntent = {
  /** 分类器用组内哪个成员 */
  classifierMember: string
  /** 分类超时（毫秒） */
  timeoutMs: number
  /** 分类结果缓存时长（毫秒） */
  cacheMs: number
  /** 意图档位 */
  levels: IntentLevel[]
}

const LEVEL_ID_MAX = 32
const LABEL_MAX = 200

/**
 * 意图分类配置区。
 *
 * ── 它做什么 ────────────────────────────────────────────────
 * 用户回合开始时，用组内某个成员（通常是小而快的那个）问一次「这条消息有多难」，
 * 再把候选收窄到那一档列出的成员。
 *
 * ── 为什么分类器是「组内成员」而不是「随便哪个模型」 ──────────────
 * 分类器的开销算在组上（magpie 的做法），走组内账号 —— 与用户请求受同一套
 * 限制器规则约束，不会出现「分类器被别的家的限额规则挡下」这种怪状况。
 *
 * ── 失败语义 ────────────────────────────────────────────────
 * 分类失败/超时/答非所问一律**不分类**，按原候选走。分类器坏了不拖垮请求。
 */
export function RoutingIntentEditor({ group, locked, update }: {
  group: RouteGroup
  locked: boolean
  update(mutate: (group: RouteGroup) => void): void
}) {
  const intent = group.intent
  const enabled = Boolean(intent)
  const members = group.members.filter(member => member.included)

  const toggle = () => update(next => {
    if (next.intent) {
      next.intent = undefined
      return
    }
    // 开启时给一个可用的起点：分类器用第一个参与成员，档位就用参与成员各一档
    const included = next.members.filter(member => member.included)
    const classifier = included[0]?.id || ''
    next.intent = {
      classifierMember: classifier,
      timeoutMs: 8000,
      cacheMs: 600000,
      levels: included.length
        ? included.map(member => ({ id: member.id, label: '', members: [member.id] }))
        : [],
    }
  })

  const patch = (mutate: (intent: RouteIntent) => void) => update(next => {
    if (!next.intent) return
    const draft = { ...next.intent, levels: next.intent.levels.map(level => ({ ...level, members: [...level.members] })) }
    mutate(draft)
    next.intent = draft
  })

  const addLevel = () => patch(draft => {
    const id = `level-${draft.levels.length + 1}`
    draft.levels.push({ id, label: '', members: [] })
  })

  const removeLevel = (index: number) => patch(draft => {
    draft.levels.splice(index, 1)
  })

  const moveLevel = (index: number, direction: number) => patch(draft => {
    const target = index + direction
    if (target < 0 || target >= draft.levels.length) return
    const [row] = draft.levels.splice(index, 1)
    draft.levels.splice(target, 0, row)
  })

  if (!intent) {
    return <div className='routing-intent'>
      <div className='routing-rule-field'>
        <Label>{t('意图分类')}</Label>
        <div className='routing-intent-off'>
          <span className='detail'>{t('开启后：网关先用组内一个小模型判断这条消息有多难，再自动选对应档位的模型。')}</span>
          <Button size='sm' variant='outline' disabled={locked || members.length === 0} onClick={toggle}>
            {t('开启意图分类')}
          </Button>
        </div>
      </div>
    </div>
  }

  return <div className='routing-intent'>
    <div className='routing-rule-field'>
      <Label>{t('意图分类')}</Label>
      <div className='routing-intent-off'>
        <span className='detail'>{t('已开启。分类失败时自动按原候选转发，不影响请求成功。')}</span>
        <Button size='sm' variant='ghost' disabled={locked} onClick={toggle}>{t('关闭')}</Button>
      </div>
    </div>

    <div className='routing-rule-field'>
      <Label>{t('分类器模型')}</Label>
      <RoutingSelect label={t('分类器模型')} value={intent.classifierMember} disabled={locked}
        options={members.map(member => [member.id, `${member.id} · ${member.model}`] as [string, string])}
        onChange={value => patch(draft => { draft.classifierMember = value })} />
      <span className='detail'>{t('用组内这个成员来判断难易 —— 建议选一个快而便宜的模型。')}</span>
    </div>

    <div className='routing-intent-levels'>
      <div className='routing-intent-levels-head'>
        <Label>{t('意图档位')}</Label>
        <Button size='xs' variant='outline' disabled={locked} onClick={addLevel}>{t('添加档位')}</Button>
      </div>
      <span className='detail'>{t('档位按顺序编号给分类器看；每一档写清楚「什么样的请求属于这一档」，命中后只走该档列出的成员。')}</span>
      {intent.levels.length === 0 ? <p className='text-destructive'>{t('至少需要一个档位，否则不会启用分类。')}</p> : null}
      {intent.levels.map((level, index) => (
        <div className='routing-intent-level' key={level.id}>
          <div className='routing-intent-level-head'>
            <span className='routing-intent-level-no'>{index + 1}</span>
            <Input value={level.id} disabled={locked} maxLength={LEVEL_ID_MAX}
              aria-label={t('档位标识')}
              onChange={event => patch(draft => { draft.levels[index].id = event.currentTarget.value.trim() })} />
            <Button size='xs' variant='ghost' disabled={locked || index === 0} onClick={() => moveLevel(index, -1)}>{t('上移')}</Button>
            <Button size='xs' variant='ghost' disabled={locked || index === intent.levels.length - 1} onClick={() => moveLevel(index, 1)}>{t('下移')}</Button>
            <Button size='xs' variant='ghost' disabled={locked} onClick={() => removeLevel(index)}>{t('删除')}</Button>
          </div>
          <Input value={level.label} disabled={locked} maxLength={LABEL_MAX}
            placeholder={t('例如：a greeting or a small question; little thought needed')}
            aria-label={t('档位描述')}
            onChange={event => patch(draft => { draft.levels[index].label = event.currentTarget.value })} />
          <div className='routing-intent-level-members'>
            <span className='detail'>{t('命中这一档时走这些成员：')}</span>
            {members.map(member => {
              const on = level.members.includes(member.id)
              return <label key={member.id} className='routing-intent-member'>
                <input type='checkbox' disabled={locked} checked={on}
                  onChange={event => patch(draft => {
                    const list = draft.levels[index].members
                    draft.levels[index].members = event.currentTarget.checked
                      ? [...list, member.id]
                      : list.filter(id => id !== member.id)
                  })} />
                <span>{member.id} · {member.model}</span>
              </label>
            })}
          </div>
        </div>
      ))}
    </div>

    <div className='routing-rule-field'>
      <Label>{t('分类超时（毫秒）')}</Label>
      <Input type='number' min={1000} max={120000} step={1000} disabled={locked}
        value={String(intent.timeoutMs)}
        onChange={event => patch(draft => {
          const value = Number(event.currentTarget.value)
          draft.timeoutMs = Number.isFinite(value) && value > 0 ? Math.min(120000, Math.max(1000, Math.round(value))) : 8000
        })} />
      <span className='detail'>{t('超时后不分类，按原候选转发（1000~120000）。')}</span>
    </div>

    <div className='routing-rule-field'>
      <Label>{t('分类结果缓存（毫秒）')}</Label>
      <Input type='number' min={0} max={3600000} step={60000} disabled={locked}
        value={String(intent.cacheMs)}
        onChange={event => patch(draft => {
          const value = Number(event.currentTarget.value)
          draft.cacheMs = Number.isFinite(value) && value >= 0 ? Math.min(3600000, Math.round(value)) : 0
        })} />
      <span className='detail'>{t('同一句话在这个时长内不重复问分类器（0 = 不缓存）。')}</span>
    </div>
  </div>
}

/** 档位的默认描述建议（给「添加档位」用）。 */
export function suggestLevelLabel(member: RouteMember): string {
  return `${member.id}: `
}
