use std::{
    fmt::Debug,
    hash::Hash,
    marker::PhantomData,
    ops::{Add, AddAssign, Sub},
    sync::Arc,
};

use fxhash::FxHashMap;

use crate::{
    ConstraintSet, DifferentialHeuristic, GeneralizedSippConfig, Heuristic, Interval, LandmarkSet,
    LimitValues, SafeIntervalPathPlanning, SippConfig, SippState, SippStats, SippTask, Solution,
    State, Task, TransitionSystem,
};

/// Implementation of Safe Interval Path Planning algorithm that supports landmarks
/// (or positive constraints) to visit before aiming for the goal state.
pub struct SafeIntervalPathPlanningWithLandmarks<TS, S, A, C, DC, H>
where
    TS: TransitionSystem<S, A, C, DC>,
    S: State + Debug + Hash + Eq + Clone,
    A: Debug + Copy,
    C: Debug
        + Hash
        + Eq
        + PartialOrd
        + Ord
        + Add<DC, Output = C>
        + Sub<DC, Output = C>
        + Sub<C, Output = DC>
        + Copy
        + Default
        + LimitValues,
    DC: Debug + Ord + Sub<DC, Output = DC> + Copy + Default,
    H: Heuristic<TS, S, A, C, DC>,
{
    sipp: SafeIntervalPathPlanning<TS, S, A, C, DC, DifferentialHeuristic<TS, S, A, C, DC, H>>,
    solutions: Vec<Solution<Arc<SippState<S, C>>, A, C, DC>>,
    solution_parts:
        FxHashMap<((Arc<SippState<S, C>>, C), usize), Solution<Arc<SippState<S, C>>, A, C, DC>>,
    landmark_states: Vec<Arc<SippState<S, C>>>,
    landmark_times: Vec<C>,
    stats: LSippStats,
}

impl<TS, S, A, C, DC, H> SafeIntervalPathPlanningWithLandmarks<TS, S, A, C, DC, H>
where
    TS: TransitionSystem<S, A, C, DC>,
    S: State + Debug + Hash + Eq + Clone,
    A: Debug + Copy,
    C: Debug
        + Hash
        + Eq
        + PartialOrd
        + Ord
        + Add<DC, Output = C>
        + Sub<DC, Output = C>
        + Sub<C, Output = DC>
        + Copy
        + Default
        + LimitValues,
    DC: Debug + Ord + Sub<DC, Output = DC> + Copy + Default,
    H: Heuristic<TS, S, A, C, DC>,
{
    /// Creates a new instance of the Safe Interval Path Planning algorithm with landmarks.
    pub fn new(transition_system: Arc<TS>) -> Self {
        Self {
            sipp: SafeIntervalPathPlanning::new(transition_system),
            solution_parts: FxHashMap::default(),
            solutions: vec![],
            landmark_states: vec![],
            landmark_times: vec![],
            stats: LSippStats::default(),
        }
    }

    fn init(&mut self) {
        self.solutions.clear();
        self.solution_parts.clear();
        self.landmark_states.clear();
        self.landmark_times.clear();

        self.stats.searches += 1;
    }

    /// Attempts to solve the given configuration, and returns the solution if any.
    pub fn solve(
        &mut self,
        config: &LSippConfig<TS, S, A, C, DC, H>,
    ) -> Option<Solution<Arc<SippState<S, C>>, A, C, DC>> {
        self.init();

        let solution = if config.landmarks.is_empty() {
            // No landmarks, just solve the task with SIPP
            self.sipp.solve(&SippConfig::new(
                config.task.clone(),
                Default::default(),
                config.constraints.clone(),
                self.get_heuristic(config, config.task.clone()),
                config.precision,
            ))
        } else {
            // Solve the task with landmarks
            self.to_first_landmark(config);
            self.between_landmarks(config);
            self.to_goal(config);
            self.get_solution(config)
        };

        solution.and_then(|sol| {
            // Last move must be valid until the end of the horizon
            if sol.steps.last().unwrap().0.safe_interval.end != C::max_value() {
                None
            } else {
                Some(sol)
            }
        })
    }

    // Go from the initial state to the first landmark
    fn to_first_landmark(&mut self, config: &LSippConfig<TS, S, A, C, DC, H>) {
        let task = Arc::new(Task::new(
            config.task.initial_state.clone(),
            config.landmarks[0].state.clone(),
            config.task.initial_cost,
        ));
        let config = self.sipp.to_generalized(&SippConfig::new(
            task.clone(),
            config.landmarks[0].interval,
            config.constraints.clone(),
            self.get_heuristic(config, task),
            config.precision,
        ));

        if config.is_none() {
            return;
        }

        let config = config.unwrap();

        self.solutions = self.sipp.solve_generalized(&config);

        self.store_solution_parts(0);
    }

    // Connect all landmarks sequentially
    fn between_landmarks(&mut self, config: &LSippConfig<TS, S, A, C, DC, H>) {
        for (i, landmark) in config.landmarks.iter().enumerate().skip(1) {
            let task = Arc::new(Task::new(
                config.landmarks[i - 1].state.clone(),
                landmark.state.clone(),
                config.task.initial_cost,
            ));
            let config = GeneralizedSippConfig::new(
                SippTask::new(
                    self.landmark_times.drain(..).collect(),
                    self.landmark_states.drain(..).collect(),
                    landmark.state.clone(),
                    landmark.interval,
                    task.clone(),
                ),
                config.constraints.clone(),
                self.get_heuristic(config, task),
                config.precision,
            );

            self.solutions = self.sipp.solve_generalized(&config);

            self.store_solution_parts(i);
        }
    }

    // Go from the last landmark to the goal state
    fn to_goal(&mut self, config: &LSippConfig<TS, S, A, C, DC, H>) {
        let task = Arc::new(Task::new(
            config.landmarks[config.landmarks.len() - 1].state.clone(),
            config.task.goal_state.clone(),
            config.task.initial_cost,
        ));
        let config = GeneralizedSippConfig::new(
            SippTask::new(
                self.landmark_times.drain(..).collect(),
                self.landmark_states.drain(..).collect(),
                config.task.goal_state.clone(),
                Interval::default(),
                task.clone(),
            ),
            config.constraints.clone(),
            self.get_heuristic(config, task),
            config.precision,
        );

        self.solutions = self.sipp.solve_generalized(&config);
    }

    /// Stores the last solutions as solution parts
    fn store_solution_parts(&mut self, landmark: usize) {
        for solution in self.solutions.drain(..) {
            self.landmark_states
                .push(solution.steps.last().unwrap().0.clone());
            self.landmark_times.push(solution.cost);
            self.solution_parts
                .insert((solution.steps.last().unwrap().clone(), landmark), solution);
        }
    }

    /// Returns the solution to the given task, if any.
    fn get_solution(
        &mut self,
        config: &LSippConfig<TS, S, A, C, DC, H>,
    ) -> Option<Solution<Arc<SippState<S, C>>, A, C, DC>> {
        if self.solutions.is_empty() {
            return None;
        }

        let mut solution = Solution::default();
        solution.cost = self.solutions[0].cost;

        let mut current_part = self.solutions.swap_remove(0);
        for landmark in (0..(config.landmarks.len() + 1)).rev() {
            current_part
                .steps
                .drain(..)
                .rev()
                .for_each(|step| solution.steps.push(step));
            current_part
                .actions
                .drain(..)
                .rev()
                .for_each(|action| solution.actions.push(action));

            if landmark > 0 {
                current_part = self
                    .solution_parts
                    .remove(&(solution.steps.last().unwrap().clone(), landmark - 1))
                    .unwrap();
                solution.steps.pop(); // Remove the last step, as it is the first step of the next part
            }
        }

        solution.steps.reverse();
        solution.actions.reverse();

        Some(solution)
    }

    fn get_heuristic(
        &self,
        config: &LSippConfig<TS, S, A, C, DC, H>,
        task: Arc<Task<S, C>>,
    ) -> Arc<DifferentialHeuristic<TS, S, A, C, DC, H>> {
        Arc::new(DifferentialHeuristic::new(
            task,
            config.pivots.clone(),
            config.heuristic_to_pivots.clone(),
        ))
    }

    /// Returns the statistics of the search algorithm.
    pub fn get_stats(&mut self) -> LSippStats {
        self.stats.sipp_stats = self.sipp.get_stats();
        self.stats
    }
}

/// Input configuration for the Safe Interval Path Planning algorithm with landmarks.
pub struct LSippConfig<TS, S, A, C, DC, H>
where
    TS: TransitionSystem<S, A, C, DC>,
    S: State + Debug + Hash + Eq + Clone,
    A: Copy,
    C: Eq
        + PartialOrd
        + Ord
        + Add<DC, Output = C>
        + Sub<C, Output = DC>
        + Copy
        + Default
        + LimitValues,
    DC: Copy,
    H: Heuristic<TS, S, A, C, DC>,
{
    task: Arc<Task<S, C>>,
    constraints: Arc<ConstraintSet<S, C>>,
    landmarks: LandmarkSet<S, C>,
    /// A set of pivot states.
    pivots: Arc<Vec<S>>,
    /// A set of heuristics to those pivot states.
    heuristic_to_pivots: Arc<Vec<Arc<H>>>,
    precision: DC,
    _phantom: PhantomData<(TS, A)>,
}

impl<TS, S, A, C, DC, H> LSippConfig<TS, S, A, C, DC, H>
where
    TS: TransitionSystem<S, A, C, DC>,
    S: State + Debug + Hash + Eq + Clone,
    A: Copy,
    C: Eq
        + PartialOrd
        + Ord
        + Add<DC, Output = C>
        + Sub<C, Output = DC>
        + Copy
        + Default
        + LimitValues,
    DC: Copy,
    H: Heuristic<TS, S, A, C, DC>,
{
    pub fn new(
        task: Arc<Task<S, C>>,
        constraints: Arc<ConstraintSet<S, C>>,
        landmarks: LandmarkSet<S, C>,
        heuristic: Arc<H>,
        precision: DC,
    ) -> Self {
        Self {
            task: task.clone(),
            constraints,
            landmarks,
            pivots: Arc::new(vec![task.goal_state.clone()]),
            heuristic_to_pivots: Arc::new(vec![heuristic]),
            precision,
            _phantom: PhantomData,
        }
    }

    pub fn new_with_pivots(
        task: Arc<Task<S, C>>,
        constraints: Arc<ConstraintSet<S, C>>,
        landmarks: LandmarkSet<S, C>,
        pivots: Arc<Vec<S>>,
        heuristic_to_pivots: Arc<Vec<Arc<H>>>,
        precision: DC,
    ) -> Self {
        Self {
            task,
            constraints,
            landmarks,
            pivots,
            heuristic_to_pivots,
            precision,
            _phantom: PhantomData,
        }
    }
}

/// Statistics of the Safe Interval Path Planning algorithm with landmarks.
#[derive(Debug, Default, Clone, Copy)]
pub struct LSippStats {
    pub searches: usize,
    pub sipp_stats: SippStats,
}

impl AddAssign for LSippStats {
    fn add_assign(&mut self, rhs: Self) {
        self.searches += rhs.searches;
        self.sipp_stats += rhs.sipp_stats;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ordered_float::OrderedFloat;

    use crate::{
        Constraint, Graph, GraphNodeId, Interval, LSippConfig, ReverseResumableAStar,
        SafeIntervalPathPlanningWithLandmarks, SimpleEdgeData, SimpleHeuristic, SimpleNodeData,
        SimpleState, SimpleWorld, Task,
    };

    fn simple_graph(size: usize) -> Arc<Graph<SimpleNodeData, SimpleEdgeData>> {
        let mut graph = Graph::new();
        for x in 0..size {
            for y in 0..size {
                graph.add_node((x as f32, y as f32));
            }
        }
        for x in 0..size {
            for y in 0..size {
                let node_id = GraphNodeId(x + y * size);
                if x > 0 {
                    graph.add_edge(node_id, GraphNodeId(x - 1 + y * size), 1.0);
                }
                if y > 0 {
                    graph.add_edge(node_id, GraphNodeId(x + (y - 1) * size), 1.0);
                }
                if x < size - 1 {
                    graph.add_edge(node_id, GraphNodeId(x + 1 + y * size), 1.0);
                }
                if y < size - 1 {
                    graph.add_edge(node_id, GraphNodeId(x + (y + 1) * size), 1.0);
                }
            }
        }
        Arc::new(graph)
    }

    #[test]
    fn test_simple() {
        let size = 10;
        let graph = simple_graph(size);
        let transition_system = Arc::new(SimpleWorld::new(graph, 0.4));
        let mut solver = SafeIntervalPathPlanningWithLandmarks::new(transition_system.clone());

        for x in 0..size {
            for y in 0..size {
                let task = Arc::new(Task::new(
                    SimpleState(GraphNodeId(x + size * y)),
                    SimpleState(GraphNodeId(size * size - 1)),
                    OrderedFloat(0.0),
                ));
                let config = LSippConfig::new(
                    task.clone(),
                    Default::default(),
                    Default::default(),
                    Arc::new(ReverseResumableAStar::new(
                        transition_system.clone(),
                        task.clone(),
                        SimpleHeuristic::new(transition_system.clone(), Arc::new(task.reverse())),
                    )),
                    1e-6.into(),
                );
                let before = solver.get_stats();
                let solution = solver.solve(&config).unwrap();
                let after = solver.get_stats();
                assert_eq!(
                    solution.cost,
                    OrderedFloat(((size - x - 1) + (size - y - 1)) as f32)
                );
                assert_eq!(after.searches, before.searches + 1);
                assert_eq!(after.sipp_stats.searches, before.sipp_stats.searches + 1);
            }
        }
    }

    #[test]
    fn test_with_landmarks() {
        let size = 10;
        let graph = simple_graph(size);
        let transition_system = Arc::new(SimpleWorld::new(graph, 0.4));
        let mut solver = SafeIntervalPathPlanningWithLandmarks::new(transition_system.clone());

        let task = Arc::new(Task::new(
            SimpleState(GraphNodeId(0)),
            SimpleState(GraphNodeId(size * size - 1)),
            OrderedFloat(0.0),
        ));
        let config = LSippConfig::new(
            task.clone(),
            Default::default(),
            vec![
                Arc::new(Constraint::new_state_constraint(
                    0,
                    SimpleState(GraphNodeId(size - 1)),
                    Interval::default(),
                )),
                Arc::new(Constraint::new_state_constraint(
                    0,
                    SimpleState(GraphNodeId(size * (size - 1))),
                    Interval::default(),
                )),
            ],
            Arc::new(ReverseResumableAStar::new(
                transition_system.clone(),
                task.clone(),
                SimpleHeuristic::new(transition_system.clone(), Arc::new(task.reverse())),
            )),
            1e-6.into(),
        );
        let before = solver.get_stats();
        let solution = solver.solve(&config).unwrap();
        let after = solver.get_stats();
        assert_eq!(solution.cost, OrderedFloat((4 * (size - 1)) as f32));
        assert_eq!(after.searches, before.searches + 1);
        assert_eq!(after.sipp_stats.searches, before.sipp_stats.searches + 3);
    }
}
