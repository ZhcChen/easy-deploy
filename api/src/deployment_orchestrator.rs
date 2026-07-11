use std::{cmp::Reverse, collections::BTreeMap};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentMode {
    Normal,
    Force,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentAction {
    Deploy,
    Skip,
    Start,
    Stop,
    Upgrade,
    Downgrade,
    Restore,
    ApplicationCheck,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetUnitState {
    pub unit_id: i64,
    pub unit_key: String,
    pub unit_release_id: Option<i64>,
    pub release_version: Option<String>,
    pub release_version_code: Option<i64>,
    pub desired_status: String,
    pub stage_no: i64,
    pub unit_order: i64,
    pub removal_order: i64,
    pub target_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CurrentUnitNodeState {
    pub unit_id: i64,
    pub node_id: i64,
    pub runtime_status: String,
    pub active_unit_release_id: Option<i64>,
    pub active_version_code: Option<i64>,
    pub active_fingerprint: String,
    pub container_version_label: String,
}

#[derive(Debug, Clone)]
pub struct DeploymentPlanInput {
    pub app_id: i64,
    pub environment_id: i64,
    pub app_release_id: i64,
    pub config_revision_id: i64,
    pub mode: DeploymentMode,
    pub target_node_ids: Vec<i64>,
    pub target_units: Vec<TargetUnitState>,
    pub current_states: Vec<CurrentUnitNodeState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentPlanItem {
    pub unit_id: i64,
    pub unit_key: String,
    pub unit_release_id: Option<i64>,
    pub stage_no: i64,
    pub unit_order: i64,
    pub removal_order: i64,
    pub action: DeploymentAction,
    pub reason: String,
    pub target_fingerprint: String,
    pub previous_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentPlan {
    pub app_id: i64,
    pub environment_id: i64,
    pub app_release_id: i64,
    pub config_revision_id: i64,
    pub mode: DeploymentMode,
    pub target_node_ids: Vec<i64>,
    pub items: Vec<DeploymentPlanItem>,
    pub plan_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentPlanError(String);

impl std::fmt::Display for DeploymentPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DeploymentPlanError {}

#[derive(Serialize)]
struct PlanHashDocument<'a> {
    app_id: i64,
    environment_id: i64,
    app_release_id: i64,
    config_revision_id: i64,
    mode: DeploymentMode,
    target_node_ids: &'a [i64],
    items: &'a [DeploymentPlanItem],
}

pub fn build_deployment_plan(
    mut input: DeploymentPlanInput,
) -> Result<DeploymentPlan, DeploymentPlanError> {
    if input.target_node_ids.is_empty() {
        return Err(DeploymentPlanError(
            "部署环境至少需要一个目标节点".to_owned(),
        ));
    }
    input.target_node_ids.sort_unstable();
    input.target_node_ids.dedup();
    if input.target_units.is_empty() {
        return Err(DeploymentPlanError("应用版本不包含部署单元".to_owned()));
    }

    let mut current_by_unit_node = BTreeMap::new();
    for state in input.current_states {
        if current_by_unit_node
            .insert((state.unit_id, state.node_id), state)
            .is_some()
        {
            return Err(DeploymentPlanError(
                "同一部署单元和节点存在重复运行状态".to_owned(),
            ));
        }
    }
    let mut target_ids = BTreeMap::new();
    let mut items = Vec::with_capacity(input.target_units.len());
    for target in input.target_units {
        if target_ids.insert(target.unit_id, ()).is_some() {
            return Err(DeploymentPlanError("应用版本包含重复部署单元".to_owned()));
        }
        if !matches!(target.desired_status.as_str(), "active" | "disabled") {
            return Err(DeploymentPlanError(
                "部署单元目标状态必须是 active 或 disabled".to_owned(),
            ));
        }
        if target.desired_status == "active" && target.unit_release_id.is_none() {
            return Err(DeploymentPlanError("启用的部署单元缺少发布包".to_owned()));
        }
        let states = input
            .target_node_ids
            .iter()
            .filter_map(|node_id| current_by_unit_node.get(&(target.unit_id, *node_id)))
            .collect::<Vec<_>>();
        let previous_fingerprint = common_previous_fingerprint(&states);
        let (action, reason) =
            classify_action(input.mode, &target, &input.target_node_ids, &states);
        items.push(DeploymentPlanItem {
            unit_id: target.unit_id,
            unit_key: target.unit_key,
            unit_release_id: target.unit_release_id,
            stage_no: target.stage_no,
            unit_order: target.unit_order,
            removal_order: target.removal_order,
            action,
            reason,
            target_fingerprint: target.target_fingerprint,
            previous_fingerprint,
        });
    }

    items.sort_by_key(|item| match item.action {
        DeploymentAction::Stop => (
            0_i8,
            Reverse(item.stage_no),
            Reverse(item.removal_order),
            item.unit_id,
        ),
        _ => (
            1_i8,
            Reverse(-item.stage_no),
            Reverse(-item.unit_order),
            item.unit_id,
        ),
    });
    let document = PlanHashDocument {
        app_id: input.app_id,
        environment_id: input.environment_id,
        app_release_id: input.app_release_id,
        config_revision_id: input.config_revision_id,
        mode: input.mode,
        target_node_ids: &input.target_node_ids,
        items: &items,
    };
    let plan_json = serde_json::to_vec(&document)
        .map_err(|error| DeploymentPlanError(format!("生成部署计划失败: {error}")))?;
    let plan_hash = format!("{:x}", Sha256::digest(plan_json));
    Ok(DeploymentPlan {
        app_id: input.app_id,
        environment_id: input.environment_id,
        app_release_id: input.app_release_id,
        config_revision_id: input.config_revision_id,
        mode: input.mode,
        target_node_ids: input.target_node_ids,
        items,
        plan_hash,
    })
}

fn classify_action(
    mode: DeploymentMode,
    target: &TargetUnitState,
    target_node_ids: &[i64],
    states: &[&CurrentUnitNodeState],
) -> (DeploymentAction, String) {
    if target.desired_status == "disabled" {
        let already_stopped = states.len() == target_node_ids.len()
            && states.iter().all(|state| state.runtime_status == "stopped");
        return if already_stopped || states.is_empty() {
            (DeploymentAction::Skip, "部署单元已经停止".to_owned())
        } else {
            (DeploymentAction::Stop, "目标应用版本停用该单元".to_owned())
        };
    }
    if mode == DeploymentMode::Force {
        return (
            DeploymentAction::Deploy,
            "强制全量部署重新执行启用单元".to_owned(),
        );
    }
    if states.len() != target_node_ids.len() {
        if states.is_empty() {
            return (DeploymentAction::Start, "目标节点尚未部署该单元".to_owned());
        }
        return (
            DeploymentAction::Deploy,
            "部分目标节点缺少可信运行状态".to_owned(),
        );
    }
    let fully_matching = states.iter().all(|state| {
        state.runtime_status == "healthy"
            && state.active_unit_release_id == target.unit_release_id
            && state.active_fingerprint == target.target_fingerprint
            && Some(state.container_version_label.as_str()) == target.release_version.as_deref()
    });
    if fully_matching {
        return (
            DeploymentAction::Skip,
            "版本、配置、容器 label 和健康状态均与目标一致".to_owned(),
        );
    }
    if states
        .iter()
        .any(|state| !matches!(state.runtime_status.as_str(), "healthy" | "stopped"))
    {
        return (
            DeploymentAction::Deploy,
            "运行状态或健康探测不可信，不能跳过".to_owned(),
        );
    }
    if states.iter().all(|state| state.runtime_status == "stopped") {
        return (
            DeploymentAction::Restore,
            "部署单元已停止，需要恢复目标版本".to_owned(),
        );
    }
    let current_codes = states
        .iter()
        .filter_map(|state| state.active_version_code)
        .collect::<Vec<_>>();
    if let Some(target_code) = target.release_version_code {
        if current_codes.iter().all(|code| *code < target_code) {
            return (DeploymentAction::Upgrade, "目标发布包版本更高".to_owned());
        }
        if current_codes.iter().all(|code| *code > target_code) {
            return (DeploymentAction::Downgrade, "目标发布包版本更低".to_owned());
        }
    }
    (
        DeploymentAction::Deploy,
        "目标指纹、容器 label 或节点版本不一致".to_owned(),
    )
}

fn common_previous_fingerprint(states: &[&CurrentUnitNodeState]) -> String {
    let Some(first) = states.first() else {
        return String::new();
    };
    if states
        .iter()
        .all(|state| state.active_fingerprint == first.active_fingerprint)
    {
        first.active_fingerprint.clone()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(unit_id: i64, status: &str, release_id: Option<i64>) -> TargetUnitState {
        TargetUnitState {
            unit_id,
            unit_key: format!("unit-{unit_id}"),
            unit_release_id: release_id,
            release_version: release_id.map(|id| format!("1.0.{id}")),
            release_version_code: release_id,
            desired_status: status.to_owned(),
            stage_no: unit_id,
            unit_order: 1,
            removal_order: 1,
            target_fingerprint: format!("target-{unit_id}"),
        }
    }

    fn healthy(unit_id: i64, release_id: i64) -> CurrentUnitNodeState {
        CurrentUnitNodeState {
            unit_id,
            node_id: 10,
            runtime_status: "healthy".to_owned(),
            active_unit_release_id: Some(release_id),
            active_version_code: Some(release_id),
            active_fingerprint: format!("target-{unit_id}"),
            container_version_label: format!("1.0.{release_id}"),
        }
    }

    #[test]
    fn normal_skips_only_fully_matching_healthy_units() {
        let targets = vec![
            target(1, "active", Some(100)),
            target(2, "active", Some(200)),
        ];
        let mut api = healthy(1, 100);
        api.runtime_status = "unhealthy".to_owned();
        let current = vec![api, healthy(2, 200)];

        let plan = build_deployment_plan(DeploymentPlanInput {
            app_id: 1,
            environment_id: 2,
            app_release_id: 3,
            config_revision_id: 4,
            mode: DeploymentMode::Normal,
            target_node_ids: vec![10],
            target_units: targets,
            current_states: current,
        })
        .expect("build plan");

        assert_eq!(plan.items[0].action, DeploymentAction::Deploy);
        assert_eq!(plan.items[1].action, DeploymentAction::Skip);
        assert!(!plan.plan_hash.is_empty());
    }

    #[test]
    fn force_redeploys_active_units_but_stops_disabled_units() {
        let plan = build_deployment_plan(DeploymentPlanInput {
            app_id: 1,
            environment_id: 2,
            app_release_id: 3,
            config_revision_id: 4,
            mode: DeploymentMode::Force,
            target_node_ids: vec![10],
            target_units: vec![target(1, "active", Some(100)), target(2, "disabled", None)],
            current_states: vec![healthy(1, 100), healthy(2, 200)],
        })
        .expect("build force plan");

        assert_eq!(plan.items[0].unit_id, 2);
        assert_eq!(plan.items[0].action, DeploymentAction::Stop);
        assert_eq!(plan.items[1].action, DeploymentAction::Deploy);
    }

    #[test]
    fn classifies_start_upgrade_downgrade_restore_and_unknown_probe() {
        let targets = vec![
            target(1, "active", Some(100)),
            target(2, "active", Some(200)),
            target(3, "active", Some(300)),
            target(4, "active", Some(400)),
        ];
        let mut stopped = healthy(2, 150);
        stopped.runtime_status = "stopped".to_owned();
        let mut newer = healthy(3, 350);
        newer.active_version_code = Some(350);
        let mut unknown = healthy(4, 400);
        unknown.runtime_status = "unknown".to_owned();
        let plan = build_deployment_plan(DeploymentPlanInput {
            app_id: 1,
            environment_id: 2,
            app_release_id: 3,
            config_revision_id: 4,
            mode: DeploymentMode::Normal,
            target_node_ids: vec![10],
            target_units: targets,
            current_states: vec![stopped, newer, unknown],
        })
        .expect("build plan");

        assert_eq!(plan.items[0].action, DeploymentAction::Start);
        assert_eq!(plan.items[1].action, DeploymentAction::Restore);
        assert_eq!(plan.items[2].action, DeploymentAction::Downgrade);
        assert_eq!(plan.items[3].action, DeploymentAction::Deploy);
    }

    #[test]
    fn requires_every_target_node_to_be_healthy_before_skip() {
        let plan = build_deployment_plan(DeploymentPlanInput {
            app_id: 1,
            environment_id: 2,
            app_release_id: 3,
            config_revision_id: 4,
            mode: DeploymentMode::Normal,
            target_node_ids: vec![10, 11],
            target_units: vec![target(1, "active", Some(100))],
            current_states: vec![healthy(1, 100)],
        })
        .expect("build plan");

        assert_eq!(plan.items[0].action, DeploymentAction::Deploy);
        assert!(plan.items[0].reason.contains("目标节点"));
    }
}
