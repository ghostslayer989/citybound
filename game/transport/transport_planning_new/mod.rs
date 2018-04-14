use kay::World;
use compact::CVec;
use descartes::{N, P2, V2, Band, Segment, Path, FiniteCurve, Shape, SimpleShape, clipper,
                Intersect};
use monet::{RendererID, Instance};
use stagemaster::geometry::{band_to_geometry, CPath, CShape};
use itertools::Itertools;
use style::colors;
use ordered_float::OrderedFloat;

use planning_new::{Plan, GestureIntent, PlanResult, Prototype};

#[derive(Compact, Clone)]
pub struct RoadIntent {
    n_lanes_forward: u8,
    n_lanes_backward: u8,
}

impl RoadIntent {
    pub fn new(n_lanes_forward: u8, n_lanes_backward: u8) -> Self {
        RoadIntent { n_lanes_forward, n_lanes_backward }
    }
}

#[derive(Compact, Clone)]
pub enum RoadPrototype {
    Lane(LanePrototype),
    Intersection(IntersectionPrototype),
}

#[derive(Compact, Clone)]
pub struct LanePrototype(CPath);

#[derive(Compact, Clone)]
pub struct IntersectionConnector(P2, V2);

#[derive(Compact, Clone)]
pub struct IntersectionPrototype {
    shape: CShape,
    incoming: CVec<IntersectionConnector>,
    outgoing: CVec<IntersectionConnector>,
    connecting_lanes: CVec<LanePrototype>,
    timings: CVec<CVec<bool>>,
}

const LANE_WIDTH: N = 6.0;
const LANE_DISTANCE: N = 0.8 * LANE_WIDTH;
const CENTER_LANE_DISTANCE: N = LANE_DISTANCE;

pub fn calculate_prototypes(plan: &Plan) -> Vec<Prototype> {
    let gesture_intent_smooth_paths = plan.gestures
        .pairs()
        .filter_map(|(gesture_id, gesture)| match gesture.intent {
            GestureIntent::Road(ref road_intent) if gesture.points.len() >= 2 => {

                let center_points = gesture
                    .points
                    .windows(2)
                    .map(|point_pair| {
                        P2::from_coordinates((point_pair[0].coords + point_pair[1].coords) / 2.0)
                    })
                    .collect::<Vec<_>>();

                // for each straight line segment, we have first: a point called END,
                // marking the end of the circular arc that smoothes the first corner of
                // this line segment and then second: a point called START,
                // marking the beginning of the circular arc that smoothes the second corner
                // of this line segments. Also, we remember the direction of the line segment

                let mut end_start_directions = Vec::new();

                for (i, point_pair) in gesture.points.windows(2).enumerate() {
                    let first_corner = point_pair[0];
                    let second_corner = point_pair[1];
                    let previous_center_point = if i < 1 {
                        &first_corner
                    } else {
                        &center_points[i - 1]
                    };
                    let this_center_point = center_points[i];
                    let next_center_point = center_points.get(i + 1).unwrap_or(&second_corner);
                    let line_direction = (second_corner - first_corner).normalize();

                    let shorter_distance_to_first_corner =
                        (first_corner - previous_center_point).norm().min(
                            (first_corner - this_center_point).norm(),
                        );
                    let shorter_distance_to_second_corner =
                        (second_corner - this_center_point).norm().min(
                            (second_corner - next_center_point).norm(),
                        );

                    let end = first_corner + line_direction * shorter_distance_to_first_corner;
                    let start = second_corner - line_direction * shorter_distance_to_second_corner;

                    end_start_directions.push((end, start, line_direction));
                }

                let mut segments = Vec::new();
                let mut previous_point = gesture.points[0];
                let mut previous_direction = (gesture.points[1] - gesture.points[0]).normalize();

                for (end, start, direction) in end_start_directions {
                    if let Some(valid_incoming_arc) =
                        Segment::arc_with_direction(previous_point, previous_direction, end)
                    {
                        segments.push(valid_incoming_arc);
                    }

                    if let Some(valid_connecting_line) = Segment::line(end, start) {
                        segments.push(valid_connecting_line);
                    }

                    previous_point = start;
                    previous_direction = direction;
                }

                CPath::new(segments).ok().map(|path| {
                    (gesture_id, road_intent, path)
                })

            }
            _ => None,
        })
        .collect::<Vec<_>>();


    let gesture_shapes = gesture_intent_smooth_paths
        .iter()
        .map(|&(gesture_id, road_intent, ref path)| {
            let right_path = path.shift_orthogonally(
                CENTER_LANE_DISTANCE / 2.0 +
                    road_intent.n_lanes_forward as f32 * LANE_DISTANCE,
            ).unwrap_or_else(|| path.clone())
                .reverse();
            let left_path = path.shift_orthogonally(
                -(CENTER_LANE_DISTANCE / 2.0 +
                      road_intent.n_lanes_backward as f32 * LANE_DISTANCE),
            ).unwrap_or_else(|| path.clone());

            let outline_segments = left_path
                .segments()
                .iter()
                .cloned()
                .chain(Segment::line(left_path.end(), right_path.start()))
                .chain(right_path.segments().iter().cloned())
                .chain(Segment::line(right_path.end(), left_path.start()))
                .collect();

            CShape::new(CPath::new(outline_segments).expect(
                "Road outline path should be valid",
            )).expect("Road outline shape should be valid")
        })
        .collect::<Vec<_>>();

    let intersection_shapes = gesture_shapes
        .iter()
        .enumerate()
        .cartesian_product(gesture_shapes.iter().enumerate())
        .flat_map(|((i_a, shape_a), (i_b, shape_b))| {
            println!("{} {}", i_a, i_a);
            if i_a == i_b {
                vec![]
            } else {
                match clipper::clip(clipper::Mode::Intersection, shape_a, shape_b) {
                    Ok(shapes) => shapes,
                    Err(err) => {
                        println!("Intersection clipping error: {:?}", err);
                        vec![]
                    }
                }

            }
        });

    let mut intersection_prototypes: Vec<_> = intersection_shapes
        .map(|shape| {
            Prototype::Road(RoadPrototype::Intersection(IntersectionPrototype {
                shape: shape,
                incoming: CVec::new(),
                outgoing: CVec::new(),
                connecting_lanes: CVec::new(),
                timings: CVec::new(),
            }))
        })
        .collect();

    let lane_prototypes = {
        let raw_lane_paths = gesture_intent_smooth_paths.iter().enumerate().flat_map(
            |(lane_i, &(_, road_intent, ref path))| {
                (0..road_intent.n_lanes_forward)
                    .into_iter()
                    .map(|lane_i| {
                        CENTER_LANE_DISTANCE / 2.0 + lane_i as f32 * LANE_DISTANCE
                    })
                    .chain((0..road_intent.n_lanes_backward).into_iter().map(
                        |lane_i| {
                            -(CENTER_LANE_DISTANCE / 2.0 + lane_i as f32 * LANE_DISTANCE)
                        },
                    ))
                    .filter_map(|offset| path.shift_orthogonally(offset))
                    .collect::<Vec<_>>()
            },
        );

        let intersected_lane_paths = raw_lane_paths.into_iter().flat_map(|raw_lane_path| {
            let mut start_trim = 0.0f32;
            let mut end_trim = raw_lane_path.length();
            let mut cuts = Vec::new();

            for intersection in &mut intersection_prototypes {
                if let Prototype::Road(RoadPrototype::Intersection(ref mut intersection)) =
                    *intersection
                {
                    let intersection_points = (&raw_lane_path, intersection.shape.outline())
                        .intersect();
                    if intersection_points.len() >= 2 {
                        let entry_distance = intersection_points
                            .iter()
                            .map(|p| OrderedFloat(p.along_a))
                            .min()
                            .unwrap();
                        let exit_distance = intersection_points
                            .iter()
                            .map(|p| OrderedFloat(p.along_a))
                            .max()
                            .unwrap();
                        intersection.incoming.push(IntersectionConnector(
                            raw_lane_path.along(*entry_distance),
                            raw_lane_path.direction_along(*entry_distance),
                        ));
                        intersection.outgoing.push(IntersectionConnector(
                            raw_lane_path.along(*exit_distance),
                            raw_lane_path.direction_along(*exit_distance),
                        ));
                        cuts.push((*entry_distance, *exit_distance));
                    } else if intersection_points.len() == 1 {
                        if intersection.shape.contains(raw_lane_path.start()) {
                            let exit_distance = intersection_points[0].along_a;
                            intersection.outgoing.push(IntersectionConnector(
                                raw_lane_path.along(exit_distance),
                                raw_lane_path.direction_along(exit_distance),
                            ));
                            start_trim = start_trim.max(exit_distance);
                        } else if intersection.shape.contains(raw_lane_path.end()) {
                            let entry_distance = intersection_points[0].along_a;
                            intersection.incoming.push(IntersectionConnector(
                                raw_lane_path.along(entry_distance),
                                raw_lane_path.direction_along(entry_distance),
                            ));
                            end_trim = end_trim.min(entry_distance);
                        }
                    }
                } else {
                    unreachable!()
                }
            }

            cuts.sort_by(|a, b| OrderedFloat(a.0).cmp(&OrderedFloat(b.0)));

            cuts.insert(0, (-1.0, start_trim));
            cuts.push((end_trim, raw_lane_path.length() + 1.0));

            cuts.windows(2)
                .filter_map(|two_cuts| {
                    let ((_, exit_distance), (entry_distance, _)) = (two_cuts[0], two_cuts[1]);
                    raw_lane_path.subsection(exit_distance, entry_distance)
                })
                .collect::<Vec<_>>()
        });

        intersected_lane_paths
            .into_iter()
            .map(|path| {
                Prototype::Road(RoadPrototype::Lane(LanePrototype(path)))
            })
            .collect::<Vec<_>>()
    };

    intersection_prototypes
        .into_iter()
        .chain(lane_prototypes)
        .collect()
}

pub fn render_preview(
    result_preview: &PlanResult,
    renderer_id: RendererID,
    scene_id: usize,
    frame: usize,
    world: &mut World,
) {
    for (i, prototype) in result_preview.prototypes.iter().enumerate() {
        match *prototype {
            Prototype::Road(RoadPrototype::Lane(LanePrototype(ref lane_path))) => {
                let line_geometry =
                    band_to_geometry(&Band::new(lane_path.clone(), LANE_WIDTH * 0.7), 0.1);

                renderer_id.update_individual(
                    scene_id,
                    18_000 + i as u16,
                    line_geometry,
                    Instance::with_color(colors::STROKE_BASE),
                    true,
                    world,
                );
            }
            Prototype::Road(RoadPrototype::Intersection(IntersectionPrototype {
                                                            ref shape, ..
                                                        })) => {
                let outline_geometry =
                    band_to_geometry(&Band::new(shape.outline().clone(), 0.1), 0.1);

                renderer_id.update_individual(
                    scene_id,
                    18_500 + i as u16,
                    outline_geometry,
                    Instance::with_color(colors::SELECTION_STROKE),
                    true,
                    world,
                );
            }
            _ => {}
        }
    }
}