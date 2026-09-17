// 极简 immer 替代：state 是纯 JSON 数据，用 structuredClone 做草稿，
// reducer 里直接写变更式代码。避免引入 immer 依赖。

import { useReducer, type Dispatch } from 'react';

export function useImmerReducer<S, A>(
  reducer: (draft: S, action: A) => void,
  initialState: S,
): [S, Dispatch<A>] {
  return useReducer((state: S, action: A): S => {
    const draft = structuredClone(state);
    reducer(draft, action);
    return draft;
  }, initialState);
}
