use std::{
    cmp::Reverse,
    collections::{
        hash_map::Entry::{Occupied, Vacant},
        BinaryHeap, HashMap, HashSet,
    },
    fmt::Debug,
    hash::Hash,
    marker::PhantomData,
    sync::Arc,
    vec,
};

use chrono::Duration;

use crate::{Heuristic, Interval, SearchNode, Solution, State, Task, Time, TransitionSystem};

/// Implementation of the Safe Interval Path Planning algorithm that computes
/// the optimal sequence of actions to complete a given task in a given transition system,
/// while avoiding conflicts with other agents in the same environment.
pub struct SafeIntervalPathPlanning<TS, S, A, H>
where
    TS: TransitionSystem<S, A, Duration>,
    S: Debug + Copy + Hash + Eq,
    A: Copy,
    H: Heuristic<TS, S, A, Time, Duration>,
{
    transition_system: Arc<TS>,
    queue: BinaryHeap<Reverse<SearchNode<SippState<S>, Time, Duration>>>,
    distance: HashMap<Arc<SippState<S>>, Time>,
    closed: HashSet<Arc<SippState<S>>>,
    parent: HashMap<Arc<SippState<S>>, (A, Arc<SippState<S>>)>,
    _phantom: PhantomData<(A, H)>,
}

impl<TS, S, A, H> SafeIntervalPathPlanning<TS, S, A, H>
where
    TS: TransitionSystem<S, A, Duration>,
    S: State + Debug + Copy + Hash + Eq,
    A: Copy,
    H: Heuristic<TS, S, A, Time, Duration>,
{
    pub fn new(transition_system: Arc<TS>) -> Self {
        SafeIntervalPathPlanning {
            transition_system,
            queue: BinaryHeap::new(),
            distance: HashMap::new(),
            closed: HashSet::new(),
            parent: HashMap::new(),
            _phantom: PhantomData::default(),
        }
    }

    // Transforms the configuration into a generalized configuration, if any
    // safe intervals exist for the initial state.
    pub fn to_generalized(
        &self,
        config: &SippConfig<TS, S, A, H>,
        single_path: bool,
    ) -> Option<GeneralizedSippConfig<TS, S, A, H>> {
        let initial_state = config.task.initial_state();
        let initial_time = config.initial_time;
        let goal_state = config.task.goal_state();

        let safe_intervals = self.get_safe_intervals(&initial_state);
        let safe_interval = safe_intervals
            .iter()
            .find(|interval| interval.start <= initial_time && interval.end >= initial_time);

        if safe_interval.is_none() {
            return None;
        }

        let initial_state = Arc::new(SippState {
            safe_interval: *safe_interval.unwrap(),
            internal_state: initial_state,
        });

        let goal_state = Arc::new(SippState {
            safe_interval: config.interval,
            internal_state: goal_state,
        });

        let sipp_task = Arc::new(SippTask {
            initial_times: vec![initial_time],
            initial_states: vec![initial_state],
            goal_state,
            internal_task: config.task.clone(),
        });

        Some(GeneralizedSippConfig::new(
            sipp_task,
            config.heuristic.clone(),
            single_path,
        ))
    }

    /// Applies the algorithm to the given configuration.
    pub fn solve(
        &mut self,
        config: &SippConfig<TS, S, A, H>,
    ) -> Option<Solution<Arc<SippState<S>>, A, Time>> {
        self.to_generalized(config, true)
            .map(|config| self.solve_generalized(&config).pop())
            .flatten()
    }

    /// Applies the algorithm to the given configuration.
    pub fn solve_generalized(
        &mut self,
        config: &GeneralizedSippConfig<TS, S, A, H>,
    ) -> Vec<Solution<Arc<SippState<S>>, A, Time>> {
        self.init(config);
        self.find_paths(config)
            .iter()
            .map(|g| self.get_solution(g))
            .collect()
    }

    /// Initializes the search algorithm by clearing the data structures
    /// and enqueueing the initial states.
    fn init(&mut self, config: &GeneralizedSippConfig<TS, S, A, H>) {
        self.queue.clear();
        self.distance.clear();
        self.closed.clear();
        self.parent.clear();

        for (initial_time, initial_state) in config
            .task
            .initial_times
            .iter()
            .zip(config.task.initial_states.iter())
        {
            let initial_node = SearchNode {
                state: initial_state.clone(),
                cost: *initial_time,
                heuristic: Duration::seconds(0),
            };

            self.distance
                .insert(initial_node.state.clone(), initial_node.cost);
            self.queue.push(Reverse(initial_node));
        }
    }

    fn find_paths(
        &mut self,
        config: &GeneralizedSippConfig<TS, S, A, H>,
    ) -> Vec<SearchNode<SippState<S>, Time, Duration>> {
        let mut goals = vec![];

        while let Some(Reverse(current)) = self.queue.pop() {
            if current.cost > self.distance[current.state.as_ref()] {
                // A better path has already been found
                continue;
            }

            if config.task.is_goal(&current) {
                // A path to the goal has been found
                goals.push(current.clone());
                if config.single_path {
                    break;
                }
            }

            // Expand the current state and enqueue its successors
            for successor in self.get_successors(config, &current) {
                self.distance
                    .insert(successor.state.clone(), successor.cost);
                self.queue.push(Reverse(successor));
            }

            self.closed.insert(current.state.clone()); // Mark the state as closed because it has been expanded
        }

        goals
    }

    fn get_successors(
        &mut self,
        config: &GeneralizedSippConfig<TS, S, A, H>,
        current: &SearchNode<SippState<S>, Time, Duration>,
    ) -> Vec<SearchNode<SippState<S>, Time, Duration>> {
        let mut successors = vec![];

        for action in self
            .transition_system
            .actions_from(current.state.internal_state.clone())
        {
            let successor_state = Arc::new(
                self.transition_system
                    .transition(current.state.internal_state.clone(), &action),
            );
            let transition_cost = self
                .transition_system
                .transition_cost(current.state.internal_state.clone(), &action);

            let heuristic = config.heuristic.get_heuristic(successor_state.clone());
            if heuristic.is_none() {
                continue; // Goal state is not reachable from this state
            }
            let heuristic = heuristic.unwrap();

            if current.cost + transition_cost + heuristic > config.task.goal_state.safe_interval.end
            {
                // The goal state is not reachable in time
                continue;
            }

            let collision_intervals = self.get_collision_intervals(*action);

            // Try to reach any of the safe intervals of the destination state
            // and add the corresponding successors to the queue if a better path has been found
            for safe_interval in self.get_safe_intervals(successor_state.as_ref()).iter() {
                let mut successor_cost = current.cost + transition_cost;

                if successor_cost > safe_interval.end {
                    // Cannot reach this safe interval in time
                    continue;
                }

                if successor_cost < safe_interval.start {
                    // Would arrive too early
                    successor_cost = safe_interval.start; // Try to depart later to arrive at the right time
                    if successor_cost - transition_cost > current.state.safe_interval.end {
                        // Cannot depart that late from the current safe interval
                        continue;
                    }
                }

                if let Some(collision_interval) = collision_intervals
                    .iter()
                    .find(|interval| interval.end >= successor_cost - transition_cost)
                {
                    if successor_cost - transition_cost >= collision_interval.start {
                        // Collision detected
                        successor_cost = collision_interval.end + transition_cost; // Try to depart later

                        if successor_cost - transition_cost > current.state.safe_interval.end
                            || successor_cost > safe_interval.end
                        {
                            continue;
                        }
                    }
                }

                if successor_cost + heuristic > config.task.goal_state.safe_interval.end {
                    // The goal state is not reachable in time
                    continue;
                }

                // TODO: add intermediate state in case of waiting action?

                let successor_state = Arc::new(SippState {
                    safe_interval: *safe_interval,
                    internal_state: successor_state.clone(),
                });

                let successor = SearchNode {
                    state: successor_state,
                    cost: successor_cost,
                    heuristic,
                };

                let improved = match self.distance.entry(successor.state.clone()) {
                    Occupied(mut e) => {
                        if successor_cost < *e.get() {
                            *e.get_mut() = successor_cost;
                            true
                        } else {
                            false
                        }
                    }
                    Vacant(e) => {
                        e.insert(successor_cost);
                        true
                    }
                };

                if improved {
                    self.parent
                        .insert(successor.state.clone(), (*action, current.state.clone()));
                    successors.push(successor);
                }
            }
        }

        successors
    }

    fn get_solution(
        &self,
        goal: &SearchNode<SippState<S>, Time, Duration>,
    ) -> Solution<Arc<SippState<S>>, A, Time> {
        let mut solution = Solution::default();
        let mut current = goal.state.clone();

        solution.states.push(current.clone());
        solution.costs.push(self.distance[&current]);

        while let Some((action, parent)) = self.parent.get(&current) {
            current = parent.clone();
            solution.states.push(current.clone());
            solution.costs.push(self.distance[&current]);
            solution.actions.push(*action);
        }

        solution.costs.reverse();
        solution.actions.reverse();
        solution.states.reverse();

        solution
    }

    fn get_safe_intervals(&self, _state: &S) -> Vec<Interval> {
        vec![Interval::default()]
    }

    fn get_collision_intervals(&self, _action: A) -> Vec<Interval> {
        vec![]
    }
}

/// Input configuration for the Safe Interval Path Planning algorithm.
pub struct SippConfig<TS, S, A, H>
where
    TS: TransitionSystem<S, A, Duration>,
    S: State + Debug + Copy + Hash + Eq,
    A: Copy,
    H: Heuristic<TS, S, A, Time, Duration>,
{
    initial_time: Time,
    task: Arc<Task<S>>,
    interval: Interval,
    heuristic: Arc<H>,
    _phantom: PhantomData<(TS, S, A)>,
}

impl<TS, S, A, H> SippConfig<TS, S, A, H>
where
    TS: TransitionSystem<S, A, Duration>,
    S: State + Debug + Copy + Hash + Eq,
    A: Copy,
    H: Heuristic<TS, S, A, Time, Duration>,
{
    pub fn new(
        initial_time: Time,
        task: Arc<Task<S>>,
        interval: Interval,
        heuristic: Arc<H>,
    ) -> Self {
        SippConfig {
            initial_time,
            task,
            interval,
            heuristic,
            _phantom: PhantomData::default(),
        }
    }
}

/// Input configuration for the Generalized Safe Interval Path Planning algorithm.
pub struct GeneralizedSippConfig<TS, S, A, H>
where
    TS: TransitionSystem<S, A, Duration>,
    S: State + Debug + Copy + Hash + Eq,
    A: Copy,
    H: Heuristic<TS, S, A, Time, Duration>,
{
    task: Arc<SippTask<S>>,
    heuristic: Arc<H>,
    single_path: bool,
    _phantom: PhantomData<(TS, S, A)>,
}

impl<TS, S, A, H> GeneralizedSippConfig<TS, S, A, H>
where
    TS: TransitionSystem<S, A, Duration>,
    S: State + Debug + Copy + Hash + Eq,
    A: Copy,
    H: Heuristic<TS, S, A, Time, Duration>,
{
    pub fn new(task: Arc<SippTask<S>>, heuristic: Arc<H>, single_path: bool) -> Self {
        GeneralizedSippConfig {
            task,
            heuristic,
            single_path,
            _phantom: PhantomData::default(),
        }
    }
}

/// State wrapper for the Safe Interval Path Planning algorithm that extends
/// a given state definition with a safe interval.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct SippState<S>
where
    S: Debug + Eq,
{
    pub safe_interval: Interval,
    pub internal_state: Arc<S>,
}

/// Task wrapper for the Safe Interval Path Planning algorithm that extends
/// a given task definition with all SIPP states that correspong to it.
pub struct SippTask<S>
where
    S: State + Debug + Hash + Eq,
{
    initial_times: Vec<Time>,
    initial_states: Vec<Arc<SippState<S>>>,
    goal_state: Arc<SippState<S>>,
    internal_task: Arc<Task<S>>,
}

impl<S> SippTask<S>
where
    S: State + Debug + Hash + Eq,
{
    pub fn new(
        initial_times: Vec<Time>,
        initial_states: Vec<Arc<SippState<S>>>,
        goal_state: Arc<SippState<S>>,
        internal_task: Arc<Task<S>>,
    ) -> Self {
        SippTask {
            initial_times,
            initial_states,
            goal_state,
            internal_task,
        }
    }

    fn is_goal(&self, state: &SearchNode<SippState<S>, Time, Duration>) -> bool {
        state.cost >= self.goal_state.safe_interval.start
            && state.cost <= self.goal_state.safe_interval.end
            && self
                .internal_task
                .is_goal_state(state.state.internal_state.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Duration;

    use crate::{
        search::sipp::sipp::SippConfig, Graph, GraphNodeId, ReverseResumableAStar, SimpleHeuristic,
        SimpleState, SimpleWorld, Task, Time,
    };

    use super::SafeIntervalPathPlanning;

    fn simple_graph(size: usize) -> Arc<Graph> {
        let mut graph = Graph::new();
        for x in 0..size {
            for y in 0..size {
                graph.add_node((x as f64, y as f64), 1.0);
            }
        }
        for x in 0..size {
            for y in 0..size {
                let node_id = GraphNodeId(x + y * size);
                if x > 0 {
                    graph.add_edge(node_id, GraphNodeId(x - 1 + y * size), 1.0, 1.0);
                }
                if y > 0 {
                    graph.add_edge(node_id, GraphNodeId(x + (y - 1) * size), 1.0, 1.0);
                }
                if x < size - 1 {
                    graph.add_edge(node_id, GraphNodeId(x + 1 + y * size), 1.0, 1.0);
                }
                if y < size - 1 {
                    graph.add_edge(node_id, GraphNodeId(x + (y + 1) * size), 1.0, 1.0);
                }
            }
        }
        Arc::new(graph)
    }

    #[test]
    fn test_simple() {
        let size = 10;
        let graph = simple_graph(size);
        let transition_system = Arc::new(SimpleWorld::new(graph));
        let initial_time = Time::MIN_UTC.into();
        let mut solver = SafeIntervalPathPlanning::new(transition_system.clone());

        for x in 0..size {
            for y in 0..size {
                let task = Arc::new(Task::new(
                    Arc::new(SimpleState(GraphNodeId(x + size * y))),
                    Arc::new(SimpleState(GraphNodeId(size * size - 1))),
                ));
                let config = SippConfig::new(
                    initial_time,
                    task.clone(),
                    Default::default(),
                    Arc::new(ReverseResumableAStar::new(
                        transition_system.clone(),
                        task.clone(),
                        Arc::new(SimpleHeuristic::new(transition_system.clone(), task)),
                    )),
                );
                assert_eq!(
                    *solver.solve(&config).unwrap().costs.last().unwrap(),
                    initial_time
                        + Duration::milliseconds((((size - x - 1) + (size - y - 1)) * 1000) as i64)
                );
            }
        }
    }
}
