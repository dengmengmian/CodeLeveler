// RuntimeBridge 的 React 上下文：组件通过 useBridge() 发起用户操作。

import { createContext, useContext } from 'react';
import type { RuntimeBridge } from '../lib/controller';

const BridgeContext = createContext<RuntimeBridge | null>(null);

export const BridgeProvider = BridgeContext.Provider;

export function useBridge(): RuntimeBridge {
  const bridge = useContext(BridgeContext);
  if (!bridge) throw new Error('useBridge 必须在 BridgeProvider 内使用');
  return bridge;
}
