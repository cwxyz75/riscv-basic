#ifndef _OS_MATH_H
#define _OS_MATH_H
#define HUGE_VAL (__builtin_huge_val())
#define INFINITY (__builtin_inff())
#define NAN (__builtin_nanf(""))
#define M_PI 3.14159265358979323846
#define M_E 2.7182818284590452354
double sin(double); double cos(double); double tan(double);
double asin(double); double acos(double); double atan(double); double atan2(double, double);
double sinh(double); double cosh(double); double tanh(double);
double exp(double); double expm1(double); double log(double); double log10(double); double log2(double); double log1p(double);
double sqrt(double); double cbrt(double); double pow(double, double); double hypot(double, double);
double ceil(double); double floor(double); double trunc(double); double round(double);
double fabs(double); double fmod(double, double); double copysign(double, double);
double fmin(double, double); double fmax(double, double);
double ldexp(double, int); double frexp(double, int *); double modf(double, double *);
#define isnan(x) __builtin_isnan(x)
#define isinf(x) __builtin_isinf(x)
#define isfinite(x) __builtin_isfinite(x)
#endif
