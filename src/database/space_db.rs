use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::Hash;
use std::hash::Hasher;

use ironsea_table_vector::VectorTable;

use super::space::Coordinate;
use super::space::Position;
use super::space::Shape;
use super::space::Space;
use super::space_index::SpaceFields;
use super::space_index::SpaceIndex;
use super::space_index::SpaceSetIndex;
use super::space_index::SpaceSetObject;
use super::CoreQueryParameters;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SpaceDB {
    reference_space: String,
    values: Vec<Coordinate>,
    resolutions: Vec<SpaceIndex>,
}

impl SpaceDB {
    pub fn new(
        reference_space: &Space,
        mut space_objects: Vec<SpaceSetObject>,
        scales: Option<Vec<Vec<u32>>>,
        max_elements: Option<usize>,
    ) -> Self {
        //FIXME: Remove hard-coded constants for dimensions & bit length of morton codes.
        const DIMENSIONS: usize = 3;
        const CELL_BITS: usize = 10;

        let mut values = space_objects
            .iter()
            .map(|object| *object.value())
            .collect::<HashSet<_>>()
            .drain()
            .collect::<Vec<_>>();

        values.sort_unstable_by_key(|&c| c.u64());

        space_objects.iter_mut().for_each(|object| {
            // Update the values to point into the local (shorter) mapping array.
            let val = values.binary_search(object.value()).unwrap();
            object.set_value(val.into());
        });

        // Build the set of SpaceIndices.
        let mut resolutions = vec![];
        let mut indices = vec![];

        if let Some(scales) = scales {
            // We optimize scaling, by iteratively building coarser and coarser
            // indexes. Powers holds a list of bit shift to apply based on the
            // previous value.
            let mut powers = Vec::with_capacity(scales.len());

            // Limit temporary values lifetimes
            {
                // Sort by values, smaller to bigger.
                let mut exps = scales.clone();
                exps.sort_unstable_by_key(|v| v[0]);

                let mut previous = 0u32;
                for scale in exps {
                    // FIXME: Remove these assertions ASAP, and support multi-factor scaling
                    assert_eq!(scale.len(), DIMENSIONS);
                    assert!(scale[0] == scale[1] && scale[0] == scale[2]);

                    powers.push((scale[0], scale[0] - previous));
                    previous = scale[0];
                }
            }

            // Apply fixed scales
            let mut count = 0;
            for power in &powers {
                space_objects = space_objects
                    .into_iter()
                    .map(|mut o| {
                        let p = o.position().reduce_precision(power.1);
                        let mut hasher = DefaultHasher::new();
                        o.set_position(p);

                        // Hash, AFTER updating the position.
                        o.hash(&mut hasher);

                        (hasher.finish(), o)
                    })
                    .collect::<HashMap<_, SpaceSetObject>>()
                    .drain()
                    .map(|(_k, v)| v)
                    .collect();

                // Make sure we do not shift more position than available
                let shift = if count >= 31 { 31 } else { count };
                count += 1;
                indices.push((
                    SpaceSetIndex::new(
                        &VectorTable::new(space_objects.to_vec()),
                        DIMENSIONS,
                        CELL_BITS,
                    ),
                    vec![power.0, power.0, power.0],
                    shift,
                ));
            }
        } else {
            // Generate scales, following max_elements
            if let Some(max_elements) = max_elements {
                // We cannot return less that the total number of individual Ids stored
                // in the index for a full-volume query.
                let max_elements = max_elements.max(values.len());
                let mut count = 0;

                // The next index should contain at most half the number of
                // elements of the current index.
                let mut element_count_target = space_objects.len() / 2;

                // Insert Full resolution index.
                indices.push((
                    SpaceSetIndex::new(
                        &VectorTable::new(space_objects.clone()),
                        DIMENSIONS,
                        CELL_BITS,
                    ),
                    vec![count, count, count],
                    0, // Smallest value => highest resolution
                ));

                // Generate coarser indices, until we reach the expect max_element
                // values or we can't define bigger bit shift.
                loop {
                    // Make sure we do not shift more position than available
                    let shift = if count >= 31 { 31 } else { count };
                    count += 1;
                    space_objects = space_objects
                        .into_iter()
                        .map(|mut o| {
                            let p = o.position().reduce_precision(1);
                            let mut hasher = DefaultHasher::new();
                            o.set_position(p);

                            // Hash, AFTER updating the position.
                            o.hash(&mut hasher);

                            (hasher.finish(), o)
                        })
                        .collect::<HashMap<_, SpaceSetObject>>()
                        .drain()
                        .map(|(_k, v)| v)
                        .collect();

                    // Skip a resolution if it does not bring down enough the
                    // number of points. It would be a waste of space to store it.
                    if element_count_target < space_objects.len() {
                        continue;
                    } else {
                        // The next index should contain at most half the number of
                        // elements of the current index.
                        element_count_target = space_objects.len() / 2;
                    }

                    indices.push((
                        SpaceSetIndex::new(
                            &VectorTable::new(space_objects.to_vec()),
                            DIMENSIONS,
                            CELL_BITS,
                        ),
                        vec![count, count, count],
                        shift,
                    ));

                    if space_objects.len() <= max_elements || count == std::u32::MAX {
                        break;
                    }
                }

            // Generate indices as long as max is smaller than the number of point located in the whole space.
            // For each new index, reduce precision by two, and push to resolutions vectors.
            } else {
                // Generate only full-scale.
                indices.push((
                    SpaceSetIndex::new(&VectorTable::new(space_objects), DIMENSIONS, CELL_BITS),
                    vec![0, 0, 0],
                    0,
                ));
            }
        }

        // When done, go over the array, and set the threshold_volumes with Volume total / 8 * i in reverse order
        let space_volume = reference_space.volume();
        let max_shift = match indices.last() {
            None => 31,
            Some((_, _, x)) => *x,
        };

        for (index, scale, shift) in indices {
            // Compute threshold volume as Vt = V / 2^(max_shift) * 2^shift
            //  => the smaller shift is, the smaller the threshold is and the higher
            //     the resolution is.
            let volume = space_volume / f64::from(1 << (max_shift - shift));

            resolutions.push(SpaceIndex::new(volume, scale, index));
        }

        // Make sure the vector is sorted by threshold volumes, smallest to largest.
        // this means indices are sorted form highest resolution to lowest resolution.
        // default_resolution() relies on this to find the correct index.
        resolutions.sort_unstable_by(|a, b| match a.threshold().partial_cmp(&b.threshold()) {
            Some(o) => o,
            None => Ordering::Less, // FIXME: This is most likely incorrect...
        });

        SpaceDB {
            reference_space: reference_space.name().clone(),
            values,
            resolutions,
        }
    }

    pub fn name(&self) -> &String {
        &self.reference_space
    }

    // The smallest volume threshold, which is the highest resolution,  will
    // be at position 0
    pub fn highest_resolution(&self) -> usize {
        0
    }

    // The highest volume threshold, which is the lowest resolution,  will
    // be at position len - 1
    pub fn lowest_resolution(&self) -> usize {
        self.resolutions.len() - 1
    }

    // Is this Space DB empty?
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    // Returns the index to be used by default for the given volume.
    // The index chosen by default will be the one with the smallest volume
    // threshold which is greater or equal to the query volume.
    fn default_resolution(&self, volume: f64) -> usize {
        for i in 0..self.resolutions.len() {
            if volume <= self.resolutions[i].threshold() {
                debug!(
                    "Selected {:?} -> {:?} vs {:?}",
                    i,
                    self.resolutions[i].threshold(),
                    volume,
                );

                return i;
            }
        }

        debug!(
            "Selected lowest resolution -> {:?} vs {:?}",
            self.resolutions[self.lowest_resolution()].threshold(),
            volume
        );

        self.lowest_resolution()
    }

    fn find_resolution(&self, scale: &[u32]) -> usize {
        for i in 0..self.resolutions.len() {
            if scale <= self.resolutions[i].scale() {
                debug!(
                    "Selected {:?} -> {:?} vs {:?}",
                    i,
                    self.resolutions[i].scale(),
                    scale
                );

                return i;
            }
        }
        warn!(
            "Scale factors {:?} not found, using lowest resolution: {:?}",
            scale,
            self.resolutions[self.lowest_resolution()].scale()
        );

        self.lowest_resolution()
    }

    pub fn get_resolution(&self, parameters: &CoreQueryParameters) -> usize {
        let CoreQueryParameters {
            threshold_volume,
            resolution,
            ..
        } = parameters;

        // If a specific scale has been set, try to find it, otherwise use the
        // threshold volume to figure a default value, and fall back to the most
        // coarse resolution whenever nothing is specified.
        match resolution {
            None => {
                if let Some(threshold_volume) = threshold_volume {
                    self.default_resolution(*threshold_volume)
                } else {
                    self.lowest_resolution()
                }
            }
            Some(v) => self.find_resolution(v),
        }
    }

    // Convert the value back to caller's references
    fn decode(&self, mut objects: Vec<SpaceSetObject>) -> Vec<SpaceSetObject> {
        for o in &mut objects {
            o.set_value(self.values[o.value().u64() as usize]);
        }

        objects
    }

    // Search by Id, a.k.a values
    pub fn get_by_id(
        &self,
        id: usize,
        parameters: &CoreQueryParameters,
    ) -> Result<Vec<SpaceSetObject>, String> {
        // Is that ID referenced in the current space?
        if let Ok(offset) = self.values.binary_search(&id.into()) {
            let index = self.get_resolution(parameters);

            // Convert the view port to the encoded space coordinates
            let space = parameters.db.space(&self.reference_space)?;
            let view_port = parameters.view_port(space);

            // Select the objects
            let objects = self.resolutions[index]
                .find_by_value(&SpaceFields::new(self.name().into(), offset.into()));

            let mut results = if let Some(view_port) = view_port {
                objects
                    .into_iter()
                    .filter(|o| view_port.contains(o.position()))
                    .collect::<Vec<SpaceSetObject>>()
            } else {
                objects
            };

            // Convert the Value back to caller's references
            // Here we do not use decode() as we have a single id value to manage.
            for o in &mut results {
                o.set_value(id.into());
            }

            Ok(results)
        } else {
            Ok(vec![])
        }
    }

    // Search by positions defining a volume.
    pub fn get_by_positions(
        &self,
        positions: &[Position],
        parameters: &CoreQueryParameters,
    ) -> Result<Vec<SpaceSetObject>, String> {
        let index = self.get_resolution(threshold_volume, resolution);

        // FIXME: Should I do it here, or add the assumption this is a clean list?
        // Convert the view port to the encoded space coordinates
        //let space = parameters.db.space(&self.reference_space)?;
        //let view_port = parameters.view_port(space);

        // Select the objects
        let results = positions
            .iter()
            .flat_map(|position| self.resolutions[index].find(position))
            .collect::<Vec<SpaceSetObject>>();

        // Decode the Value reference
        let results = self.decode_value(results);

        Ok(results)
    }

    // Search by Shape defining a volume:
    // * Hyperrectangle (MBB),
    // * HyperSphere (radius around a point),
    // * Point (Specific position)
    pub fn get_by_shape(
        &self,
        shape: &Shape,
        parameters: &CoreQueryParameters,
    ) -> Result<Vec<SpaceSetObject>, String> {
        let index = self.get_resolution(threshold_volume, resolution);

        // Convert the view port to the encoded space coordinates
        let space = parameters.db.space(&self.reference_space)?;
        let view_port = parameters.view_port(space);

        // Select the objects
        let results = self.resolutions[index].find_by_shape(&shape, &view_port)?;

        // Decode the Value reference
        let results = self.decode_value(results);

        Ok(results)
    }
}
