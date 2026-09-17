/// Product-facing task status. The wire still uses session `status` strings;
/// this is a UI projection, not a new runtime type.
library;

enum TaskStatus {
  created,
  planning,
  running,
  waitingApproval,
  waitingInput,
  completed,
  failed,
}

extension TaskStatusLabel on TaskStatus {
  String get label => switch (this) {
        TaskStatus.created => '已创建',
        TaskStatus.planning => '规划中',
        TaskStatus.running => '运行中',
        TaskStatus.waitingApproval => '等待审批',
        TaskStatus.waitingInput => '等待输入',
        TaskStatus.completed => '已完成',
        TaskStatus.failed => '失败',
      };
}

/// Map host session status + local pending cards onto [TaskStatus].
TaskStatus deriveTaskStatus({
  required String status,
  bool hasApproval = false,
  bool hasClarification = false,
  bool sawPlan = false,
}) {
  if (hasApproval) return TaskStatus.waitingApproval;
  if (hasClarification) return TaskStatus.waitingInput;
  switch (status) {
    case 'running':
      return sawPlan ? TaskStatus.planning : TaskStatus.running;
    case 'completed':
      return TaskStatus.completed;
    case 'failed':
    case 'interrupted':
    case 'incomplete':
      return TaskStatus.failed;
    case 'blocked':
      return TaskStatus.waitingInput;
    case 'created':
      return TaskStatus.created;
    default:
      return TaskStatus.created;
  }
}
