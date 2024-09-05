use negative_impl::negative_impl;
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    error::Error,
    fmt::Display,
    future::Future,
    rc::Rc,
    time::Instant,
};

pub type Magnitude = f64;

/// Measures the rate and amplitude of events. Just create an instance via EventBuilder and start
/// feeding it events. It will do the rest. Interior mutability is used, so you can put these in
/// thread-local static variables for ease of use.
///
/// # Thread safety
///
/// This type is single-threaded. Create a separate instance for each thread.
/// The data will be merged across all threads to yield a combined report.
pub struct Event {
    bag: Rc<ObservationBag>,
}

impl Event {
    /// Observes an event with a magnitude of 1. An event that only takes observations of this kind
    /// is a counter and undergoes simplified reporting.
    pub fn observe_unit(&self) {
        self.bag.insert(1.0, 1);
    }

    pub fn observe(&self, magnitude: Magnitude) {
        self.bag.insert(magnitude, 1);
    }

    pub fn observe_many(&self, magnitude: Magnitude, count: usize) {
        self.bag.insert(magnitude, count);
    }

    pub fn observe_duration<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let start = Instant::now();

        let result = f();

        let duration = start.elapsed().as_secs_f64();

        self.bag.insert(duration, 1);

        result
    }

    pub async fn observe_duration_async<F, FF, R>(&self, f: F) -> R
    where
        F: FnOnce() -> FF,
        FF: Future<Output = R>,
    {
        let start = Instant::now();

        let result = f().await;

        let duration = start.elapsed().as_secs_f64();

        self.bag.insert(duration, 1);

        result
    }

    fn new(bag: Rc<ObservationBag>) -> Self {
        Self { bag }
    }
}

#[negative_impl]
impl !Send for Event {}
#[negative_impl]
impl !Sync for Event {}

pub struct EventBuilder {
    name: Option<Cow<'static, str>>,

    /// Upper bounds of histogram buckets to use. May be empty if histogram not meaningful.
    buckets: &'static [Magnitude],
}

impl EventBuilder {
    pub fn new() -> Self {
        Self {
            name: None,
            buckets: &[],
        }
    }

    pub fn name(mut self, name: impl Into<Cow<'static, str>>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn buckets(mut self, buckets: &'static [Magnitude]) -> Self {
        self.buckets = buckets;
        self
    }

    pub fn build(self) -> Result<Event, Box<dyn Error>> {
        let name = self.name.ok_or("name is required")?;

        let bag = BAGS.with_borrow_mut(|bags| {
            Rc::clone(
                &bags
                    .entry(name.to_string())
                    .or_insert_with(|| Rc::new(ObservationBag::new(self.buckets))),
            )
        });

        Ok(Event::new(bag))
    }
}

thread_local! {
    static BAGS: RefCell<HashMap<String, Rc<ObservationBag>>> = RefCell::new(HashMap::new());
}

/// Collects all the observations made about a particular event and processes the data for analysis.
///
/// Data from different bags of the same event is merged together to yield a combined report later.
struct ObservationBag {
    count: Cell<usize>,
    sum: Cell<Magnitude>,
    bucket_counts: RefCell<Vec<usize>>,
    bucket_magnitudes: &'static [Magnitude],
}

impl ObservationBag {
    fn insert(&self, magnitude: Magnitude, count: usize) {
        self.count.set(self.count.get() + count);
        self.sum
            .set(self.sum.get() + magnitude * (count as Magnitude));

        let mut bucket_counts = self.bucket_counts.borrow_mut();

        for (i, &bucket_magnitude) in self.bucket_magnitudes.iter().enumerate() {
            if magnitude <= bucket_magnitude {
                bucket_counts[i] += count;
                break;
            }
        }
    }

    fn new(buckets: &'static [Magnitude]) -> Self {
        Self {
            count: Cell::new(0),
            sum: Cell::new(0.0),
            bucket_counts: RefCell::new(vec![0; buckets.len()]),
            bucket_magnitudes: buckets,
        }
    }

    fn snapshot(&self) -> ObservationBagSnapshot {
        ObservationBagSnapshot {
            count: self.count.get(),
            sum: self.sum.get(),
            bucket_counts: self.bucket_counts.borrow().clone(),
            bucket_magnitudes: self.bucket_magnitudes,
        }
    }
}

struct ObservationBagSnapshot {
    count: usize,
    sum: Magnitude,
    bucket_counts: Vec<usize>,
    bucket_magnitudes: &'static [Magnitude],
}

impl ObservationBagSnapshot {
    fn merge(&mut self, other: &ObservationBagSnapshot) {
        self.count += other.count;
        self.sum += other.sum;

        // Briefest sanity check. We just assume the magnitudes are the same.
        assert!(self.bucket_counts.len() == other.bucket_counts.len());

        for (i, &other_bucket_count) in other.bucket_counts.iter().enumerate() {
            self.bucket_counts[i] += other_bucket_count;
        }
    }
}

/// A report page is a single thread's contribution to a report. Collect all the pages from all
/// the threads and you can assemble a report to show to the operator or to export.
pub struct ReportPage {
    bags: HashMap<String, ObservationBagSnapshot>,
}

/// Assembles a report page representing the latest state of observations on the current thread.
pub fn report_page() -> ReportPage {
    ReportPage {
        bags: BAGS.with_borrow(|bags| {
            bags.iter()
                .map(|(name, bag)| (name.clone(), bag.snapshot()))
                .collect()
        }),
    }
}

pub struct ReportBuilder {
    pages: Vec<ReportPage>,
}

impl ReportBuilder {
    pub fn new() -> Self {
        Self { pages: Vec::new() }
    }

    pub fn add_page(&mut self, page: ReportPage) {
        self.pages.push(page);
    }

    pub fn build(self) -> Report {
        let merged_snapshots = self.pages.into_iter().map(|page| page.bags).fold(
            HashMap::new(),
            |mut merged, bags| {
                for (name, snapshot) in bags {
                    merged
                        .entry(name.clone())
                        .or_insert_with(|| ObservationBagSnapshot {
                            count: 0,
                            sum: 0.0,
                            bucket_counts: vec![0; snapshot.bucket_counts.len()],
                            bucket_magnitudes: snapshot.bucket_magnitudes,
                        })
                        .merge(&snapshot);
                }

                merged
            },
        );

        Report {
            bags: merged_snapshots,
        }
    }
}

/// An analysis of collected data, designed for display to console output.
pub struct Report {
    bags: HashMap<String, ObservationBagSnapshot>,
}

impl Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Sort by name for consistent output.
        let mut sorted_bags: Vec<_> = self.bags.iter().collect();
        sorted_bags.sort_by_key(|(name, _)| name.as_str());

        for (name, snapshot) in sorted_bags {
            writeln!(f, "{}: {}", name, snapshot)?;
        }

        Ok(())
    }
}

impl Display for ObservationBagSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.count as Magnitude == self.sum {
            writeln!(f, "{} (counter)", self.count)?;
        } else if self.count > 0 {
            writeln!(
                f,
                "{}; sum {}; avg {}",
                self.count,
                self.sum,
                self.sum / self.count as Magnitude
            )?;
        } else {
            writeln!(f, "0")?;
            return Ok(());
        }

        if self.bucket_counts.is_empty() {
            return Ok(());
        }

        let mut buckets_cumulative = 0;
        for (i, &bucket_count) in self.bucket_counts.iter().enumerate() {
            buckets_cumulative += bucket_count;
            writeln!(
                f,
                "  bucket <= {}: {}",
                self.bucket_magnitudes[i], bucket_count
            )?;
        }

        writeln!(f, "  bucket +Inf: {}", self.count - buckets_cumulative)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn event_smoke_test() {
        clear();

        let event = EventBuilder::new()
            .name("test")
            .buckets(&[1.0, 2.0, 3.0])
            .build()
            .unwrap();

        event.observe(1.0);
        event.observe(2.0);
        event.observe(3.0);
        event.observe(4.0);
        event.observe(5.0);
        event.observe_many(1.0, 2);
        event.observe_many(2.0, 3);
        event.observe_many(3.0, 4);
        event.observe_many(4.0, 5);
        event.observe_many(5.0, 6);

        let page = report_page();

        assert_eq!(page.bags.len(), 1);

        let snapshot = page.bags.get("test").unwrap();
        assert_eq!(snapshot.count, 25);
        assert_eq!(snapshot.sum, 85.0);
        assert_eq!(snapshot.bucket_counts, vec![3, 4, 5]);

        let mut report_builder = ReportBuilder::new();
        report_builder.add_page(page);

        let report = report_builder.build();

        println!("{}", report);
    }

    #[test]
    fn counter() {
        clear();

        let event = EventBuilder::new()
            .name("test_counter")
            .buckets(&[])
            .build()
            .unwrap();

        event.observe_unit();
        event.observe_unit();
        event.observe_unit();

        let page = report_page();

        assert_eq!(page.bags.len(), 1);

        let snapshot = page.bags.get("test_counter").unwrap();
        assert_eq!(snapshot.count, 3);
        assert_eq!(snapshot.sum, 3.0);
        assert_eq!(snapshot.bucket_counts, Vec::<usize>::new());

        let mut report_builder = ReportBuilder::new();
        report_builder.add_page(page);

        let report = report_builder.build();

        println!("{}", report);
    }

    #[test]
    fn multi_page_report() {
        clear();

        let event = EventBuilder::new()
            .name("test")
            .buckets(&[1.0, 2.0, 3.0])
            .build()
            .unwrap();

        event.observe(0.0);
        event.observe(100.0);

        let other_page = thread::spawn(move || {
            let event = EventBuilder::new()
                .name("test")
                .buckets(&[1.0, 2.0, 3.0])
                .build()
                .unwrap();

            event.observe(-10.0);
            event.observe(1.0);
            event.observe(1.0);
            event.observe(1.0);

            report_page()
        })
        .join()
        .unwrap();

        let this_page = report_page();

        let mut report_builder = ReportBuilder::new();
        report_builder.add_page(this_page);
        report_builder.add_page(other_page);

        let report = report_builder.build();

        let snapshot = report.bags.get("test").unwrap();

        assert_eq!(snapshot.count, 6);
        assert_eq!(snapshot.sum, 93.0);
        assert_eq!(snapshot.bucket_counts, vec![5, 0, 0]);

        println!("{}", report);
    }

    #[test]
    fn multi_metric_report() {
        clear();

        let event = EventBuilder::new()
            .name("test")
            .buckets(&[1.0, 2.0, 3.0])
            .build()
            .unwrap();

        event.observe(0.0);
        event.observe(100.0);

        let event = EventBuilder::new().name("another_test").build().unwrap();

        event.observe(1234.0);
        event.observe(45678.0);

        let event = EventBuilder::new().name("more").build().unwrap();

        event.observe(1234.0);
        event.observe(45678.0);

        let mut report_builder = ReportBuilder::new();
        report_builder.add_page(report_page());

        let report = report_builder.build();

        assert_eq!(3, report.bags.len());

        println!("{}", report);
    }

    fn clear() {
        BAGS.with_borrow_mut(|bags| bags.clear());
    }
}