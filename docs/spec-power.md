# Specification — `bench power` Subcommand

The `bench power` subcommand performs offline statistical power analysis, required sample size per arm calculations, and Minimum Detectable Effect (MDE delta) estimations for two-proportion z-tests.

---

## 1. CLI Command Structure

```bash
cargo run --quiet -- bench power [OPTIONS]
```

### Options

| Flag | Type | Default | Description |
|---|---|---|---|
| `--baseline-rate <p>` | `float` | | Baseline resolved rate (e.g. `0.35`). Optional if `--from-sweep` is specified. |
| `--delta <pp>` | `float` | | Target difference in absolute percentage points (e.g. `0.05` for 5pp). Mode A (Sample Size Solver). |
| `--n <n>` | `int` | | Sample size per arm. Mode B (MDE Solver). |
| `--from-sweep <path>` | `path` | | Path to a sweep directory. Reads `evaluation.json` or `results.json` to calculate the baseline resolved rate (`resolved_count / total_instances`). |
| `--alpha <alpha>` | `float` | `0.05` | Significance level (Type I error rate). |
| `--power <power>` | `float` | `0.80` | Target statistical power (Type II error rate = $1 - \text{power}$). |
| `--one-sided` | `bool` | `false` | If set, performs a one-sided hypothesis test instead of a two-sided test. |
| `--arms <arms>` | `int` | `2` | Number of arms in the study design. If $> 2$, alpha is adjusted using the Bonferroni correction: $\alpha_{\text{adjusted}} = \frac{\alpha}{\text{arms} - 1}$. |
| `--cost-per-instance <usd>` | `float` | | Cost in USD per single run instance to calculate total study cost. |
| `--from-forecast <path>` | `path` | | Path to a forecast JSON report. Computes the average cost per instance as `total_cost_usd / target_n`, then calculates study cost. |
| `--format <format>` | `string` | `text` | Output format: `text` (human ASCII comfy-table) or `json`. |

---

## 2. Mathematical Engine

### Cohen's $h$
Cohen's $h$ is a measure of effect size for two proportions:
$$h(p_1, p_2) = 2 |\arcsin\sqrt{p_1} - \arcsin\sqrt{p_2}|$$

### Normal Distribution Approximations
- **CDF ($\Phi$)**: Calculated using a high-precision rational approximation:
  $$\Phi(x) = 1 - \phi(x) (b_1 t + b_2 t^2 + b_3 t^3 + b_4 t^4 + b_5 t^5)$$
  where $t = 1 / (1 + p x)$ and $p, b_1..b_5$ are standard rational constants.
- **Inverse CDF ($\Phi^{-1}$)**: Acklam's high-precision algorithm divides the domain into a central region ($0.02425 \le p \le 0.97575$) and tail regions, utilizing distinct rational polynomials to achieve double-precision limits.

### Solvers

- **Mode A (Sample Size)**:
  Finds the minimum integer $N$ per arm that achieves at least the target power $\text{power}_0$:
  $$\text{power}(N) \ge \text{power}_0$$
  using a numerical search initialized by the analytical approximation:
  $$N \approx 2 \left( \frac{z_{\text{crit}} + z_{\text{power}}}{h} \right)^2$$
- **Mode B (Minimum Detectable Effect)**:
  Uses bisection search over the effect size $h$ to locate the exact $h$ satisfying:
  $$\text{power}(N, h) = \text{power}_0$$
  and converts $h$ back to absolute delta:
  $$\delta = |p_1 - p_2|$$

---

## 3. Cost Estimations

### Fixed Cost
$$\text{Total Cost} = N \times \text{arms} \times \text{cost-per-instance}$$

### Forecast Pointer Parsing
When `--from-forecast <path>` is supplied, the subcommand parses:
1. `target_n` from `/forecast/target_n`
2. `point` estimate from `/forecast/total_cost_usd/point`

Calculating the unit cost:
$$\text{Unit Cost} = \frac{\text{point}}{\text{target_n}}$$
which is then multiplied by $N \times \text{arms}$ to produce the final estimate.

---

## 4. Output Schemas

### JSON Output Schema

```json
{
  "baseline_rate": 0.5,
  "delta": 0.1,
  "n": null,
  "alpha": 0.05,
  "power": 0.8,
  "one_sided": false,
  "arms": 2,
  "bonferroni_note": null,
  "solved_n": 388,
  "solved_mde": null,
  "cost_per_instance": 2.5,
  "total_cost": 1940.0
}
```
