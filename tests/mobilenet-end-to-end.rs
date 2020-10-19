#![cfg(feature = "tvm")]
#![cfg(feature = "cplex")]

use egg::EGraph;
use egg::Pattern;
use egg::Runner;
use egg::Searcher;
use glenside::extraction::ilp::create_generic_egraph_lp_model;
use glenside::extraction::ilp::into_recexpr;
use glenside::language::rewrites::PadLocation;
use glenside::language::rewrites::PadSliceStrategy;
use glenside::language::Language;
use glenside::language::MyAnalysis;
use glenside::language::PadType;
use log::info;
use rplex::Constraint;
use rplex::ConstraintType;
use rplex::Env;
use rplex::ObjectiveType;
use rplex::WeightedVariable;
use std::collections::HashMap;
use std::path::PathBuf;

#[test]
fn mobilenet_end_to_end() {
    test_logger::ensure_env_logger_initialized();

    let filename = PathBuf::from(format!(
        "{}/models/mobilenet.relay",
        env!("CARGO_MANIFEST_DIR")
    ));
    let relay = std::fs::read_to_string(&filename).unwrap();
    let module = tvm::ir::module::IRModule::parse("", relay);
    info!("parsed relay source to IRModule");

    let (expr, shapes_vec) = glenside::language::from_relay::from_relay(&module, true);
    info!("ingested Relay code into Glenside");

    let mut env = HashMap::default();
    for (k, v) in &shapes_vec {
        env.insert(k.clone(), v.clone());
    }

    let mut egraph = EGraph::new(MyAnalysis {
        name_to_shape: env.clone(),
    });
    let id = egraph.add_expr(&expr);

    let rws = vec![
        glenside::language::rewrites::flatten_unflatten_any_access(),
        glenside::language::rewrites::bubble_reshape_through_cartesian_product(),
        glenside::language::rewrites::bubble_reshape_through_compute_dot_product(),
        glenside::language::rewrites::bubble_access_concatenate_through_access_cartesian_product_not_item_axis_left(),
        glenside::language::rewrites::bubble_access_concatenate_through_access_cartesian_product_not_item_axis_right(),
        glenside::language::rewrites::bubble_access_concatenate_through_access_cartesian_product_same_item_axis(),
        glenside::language::rewrites::bubble_access_concatenate_through_compute_dot_product_item_axis(),
        glenside::language::rewrites::bubble_access_concatenate_through_compute_dot_product_not_item_axis(),
        glenside::language::rewrites::bubble_access_slice_through_access_pad_inequal_axes(),
        glenside::language::rewrites::systolic_array_with_blocking(64,64),
        glenside::language::rewrites::pad_slice_accesses(
            0,
            PadSliceStrategy::PadToClosestMultipleOf {
                multiple_of: 64,
                pad_location: PadLocation::End,
                pad_type: PadType::ZeroPadding,
            },
        ),
        glenside::language::rewrites::pad_slice_accesses(
            1,
            PadSliceStrategy::PadToClosestMultipleOf {
                multiple_of: 64,
                pad_location: PadLocation::End,
                pad_type: PadType::ZeroPadding,
            },
        ),
        glenside::language::rewrites::bubble_access_slice_through_access_cartesian_product_not_item_axis_left(),
        glenside::language::rewrites::bubble_access_slice_through_access_cartesian_product_not_item_axis_right(),
        glenside::language::rewrites::bubble_access_slice_through_access_cartesian_product_same_item_axis(),
        glenside::language::rewrites::bubble_access_slice_through_compute_dot_product_not_item_axis(),
        glenside::language::rewrites::bubble_access_slice_through_compute_dot_product_item_axis_not_tuple_axis(),
    ];

    let runner = Runner::<_, _, ()>::new(MyAnalysis::default())
        .with_egraph(egraph)
        .with_time_limit(std::time::Duration::from_secs(10))
        .with_node_limit(500000)
        .with_iter_limit(40)
        .run(&rws);

    runner.print_report();
    info!("rewrites complete");

    let env = Env::new().unwrap();
    let mut model = create_generic_egraph_lp_model(&env, &runner.egraph, &[id], "mobilenet");

    info!("setting costs for different nodes");
    let target_systolic_array_configuration = (64, 64);
    const INFINITY_VALUE: f64 = 10000.0;
    fn cost(
        enode: &Language,
        egraph: &EGraph<Language, MyAnalysis>,
        systolic_array_configuration: (usize, usize),
        infinity_value: f64,
    ) -> f64 {
        match enode {
            &Language::SystolicArray([rows_id, cols_id, _tensor_0_id, _tensor_1_id])
            | &Language::SystolicArrayWithBlocking([rows_id, cols_id, _tensor_0_id, _tensor_1_id])
                if (
                    MyAnalysis::get_usize(rows_id, egraph),
                    MyAnalysis::get_usize(cols_id, egraph),
                ) != systolic_array_configuration =>
            {
                infinity_value
            }

            Language::Symbol(_)
            | Language::AccessLiteral(_)
            | Language::Literal(_)
            | Language::NotNanFloat64(_)
            | Language::SystolicArray(_)
            | Language::SystolicArrayWithBlocking(_)
            | Language::Usize(_)
            | Language::AccessSlice(_)
            | Language::AccessConcatenate(_)
            | Language::AccessPad(_)
            | Language::AccessWindows(_)
            | Language::PadType(_)
            | Language::Access(_)
            | Language::AccessTensor(_)
            | Language::ShapeOf(_)
            | Language::ShapeRemoveAxis(_)
            | Language::ShapeInsertAxis(_)
            | Language::Shape(_)
            | Language::AccessSqueeze(_)
            | Language::AccessCartesianProduct(_)
            | Language::AccessFlatten(_)
            | Language::AccessReshape(_)
            | Language::AccessShiftRight(_)
            | Language::AccessInsertAxis(_)
            | Language::AccessBroadcast(_)
            | Language::AccessShape(_)
            | Language::List(_)
            | Language::SliceShape(_)
            | Language::AccessPair(_)
            // We don't penalize Compute, though we don't want to extract
            // compute statements. Instead, we penalize most ComputeTypes, and
            // let some types pass through until we've implemented some other
            // way to handle them.
            // TODO(@gussmith23) We shouldn't have to extract ANY computes!
            | Language::Compute(_)
            | Language::AccessTranspose(_) => 1.0,

            // Penalaize specific compute types. In the future, these constructs
            // shouldn't be extractable at all.
            // TODO(@gussmith23) We shouldn't have to extract ANY computes!
            Language::ComputeType(t) => match t {
                glenside::language::ComputeType::DotProduct => infinity_value,
                glenside::language::ComputeType::ReduceSum => 1.0,
                glenside::language::ComputeType::ReLU => 1.0,
                glenside::language::ComputeType::Sqrt => 1.0,
                glenside::language::ComputeType::Negative => 1.0,
                glenside::language::ComputeType::ElementwiseAdd => 1.0,
                glenside::language::ComputeType::ElementwiseMul => 1.0,
                glenside::language::ComputeType::ElementwiseDiv => 1.0,
                glenside::language::ComputeType::ReduceMax => 1.0,
                glenside::language::ComputeType::Softmax => 1.0,
                glenside::language::ComputeType::ReduceMean => 1.0,
            }

            // Hack constructs that we temporarily need.
            Language::BatchNormInference(_) => 1.0,

            // Old constructs.
            Language::MoveAxis(_)
            | Language::CartesianProduct(_)
            | Language::ElementwiseAdd(_)
            | Language::BsgSystolicArray(_)
            | Language::MapDotProduct(_)
            | Language::Slice(_)
            | Language::Concatenate(_) => panic!(),

        }
    }
    let mut costs = Constraint::new(
        ConstraintType::Eq, /*ignored*/
        0.0,                /*ignored*/
        "costs",
    );
    for (enode, var) in model.bn_vars.iter_mut() {
        costs.add_wvar(WeightedVariable::new_idx(
            *var,
            cost(
                enode,
                model.egraph,
                target_systolic_array_configuration,
                INFINITY_VALUE,
            ),
        ));
    }
    model
        .problem
        .set_objective(ObjectiveType::Minimize, costs)
        .unwrap();
    info!("objective set");

    info!("ilp problem created");

    let result = model.problem.solve().unwrap();
    info!("ilp problem solved");

    assert!(result.objective > 0.0);

    let expr = into_recexpr(&model, &result.variables);
    info!("Glenside expression extracted using solution of ILP problem");

    println!("{}", expr.pretty(80));
}