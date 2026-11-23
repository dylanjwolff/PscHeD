/* fmod and fmodf gained a GLIBC_2.38 version when glibc dropped their SVID
 * error handling; programs linked against a newer host libm reference it.
 * Both versions return the same values and set errno to EDOM for the same
 * inputs, so the new version forwards to the 2.35 one. The build links this
 * file into libm.so with an empty GLIBC_2.38 node in its version map. */
double fmod(double, double);
float fmodf(float, float);

double rsched_fmod_2_38(double x, double y) { return fmod(x, y); }
float rsched_fmodf_2_38(float x, float y) { return fmodf(x, y); }
__asm__(".symver rsched_fmod_2_38, fmod@GLIBC_2.38");
__asm__(".symver rsched_fmodf_2_38, fmodf@GLIBC_2.38");
