import { describe, expect, it } from 'vitest';
import { choiceOrdinal, splitChoiceOption } from './choiceLabel';

describe('splitChoiceOption', () => {
  it('strips a leading A. so the UI does not show A twice', () => {
    expect(splitChoiceOption('A. 发起权限请求')).toEqual({
      ordinal: 'A',
      body: '发起权限请求',
    });
    expect(splitChoiceOption('B) 审查工作区改动')).toEqual({
      ordinal: 'B',
      body: '审查工作区改动',
    });
  });

  it('leaves unstructured options intact', () => {
    expect(splitChoiceOption('其他')).toEqual({ ordinal: null, body: '其他' });
  });

  it('does not invent fields from the rest of the string', () => {
    const { body } = splitChoiceOption('C. 其他：具体说明要审批的对象');
    expect(body).toBe('其他：具体说明要审批的对象');
  });
});

describe('choiceOrdinal', () => {
  it('uses A/B/C for unlabeled options', () => {
    expect(choiceOrdinal(0)).toBe('A');
    expect(choiceOrdinal(2)).toBe('C');
  });
});
